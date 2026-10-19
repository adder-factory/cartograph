use std::{collections::BTreeSet, time::Duration};

use cartograph_domain::{GenerationId, ProjectId, SymbolId};
use serde::{Deserialize, Serialize};

mod native;
use thiserror::Error;

const ISSUE_HISTORY_TIMEOUT: Duration = Duration::from_mins(5);
const MAXIMUM_ATTRIBUTIONS: usize = 500_000;
const MAXIMUM_PEERS: u16 = 500;
const MAXIMUM_COMMIT_GROUPS: usize = 64;

/// How one issue-tagged commit affected a current symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueAttributionKind {
    /// The commit modified an existing symbol.
    Modified,
    /// The commit introduced the symbol.
    Added,
    /// The commit removed the historical symbol.
    Removed,
}

impl IssueAttributionKind {}

/// One bounded candidate produced by immutable Git-source comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolIssueAttribution {
    symbol_id: SymbolId,
    issue_number: u64,
    commit_sha: String,
    kind: IssueAttributionKind,
}

/// Validated inputs for one symbol attribution.
pub struct SymbolIssueAttributionInput {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Positive issue number parsed from the tagged commit.
    pub issue_number: u64,
    /// Commit sha for this record.
    pub commit_sha: String,
    /// Kind for this record.
    pub kind: IssueAttributionKind,
}

impl SymbolIssueAttribution {
    /// Creates a validated symbol issue attribution.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue number is zero/out of native storage range or
    /// `commit_sha` is not a valid bounded hexadecimal commit identity.
    pub fn new(input: SymbolIssueAttributionInput) -> Result<Self, IssueHistoryError> {
        if input.issue_number == 0
            || input.issue_number > 9_223_372_036_854_775_807_u64
            || !valid_commit_sha(&input.commit_sha)
        {
            return Err(IssueHistoryError::InvalidInput);
        }
        Ok(Self {
            symbol_id: input.symbol_id,
            issue_number: input.issue_number,
            commit_sha: input.commit_sha,
            kind: input.kind,
        })
    }
}

/// Atomic replacement request for one current generation's issue evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueHistoryRefreshMetadata {
    head_commit: String,
    commits_scanned: u64,
    tagged_commits: u64,
    oversized_commits_skipped: u64,
    comparison_failures_skipped: u64,
    truncated: bool,
}

/// Git-scan provenance for one issue-history refresh.
pub struct IssueHistoryRefreshMetadataInput {
    /// Head commit for this record.
    pub head_commit: String,
    /// Number of commits scanned.
    pub commits_scanned: u64,
    /// Number of tagged commits.
    pub tagged_commits: u64,
    /// Number of oversized commits skipped.
    pub oversized_commits_skipped: u64,
    /// Number of comparison failures skipped.
    pub comparison_failures_skipped: u64,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
}

impl IssueHistoryRefreshMetadata {
    /// Validates provenance and counters for one issue-history refresh.
    ///
    /// # Errors
    ///
    /// Returns an error if the head commit is invalid or tagged/skipped counters
    /// exceed the commit totals they describe.
    pub fn new(input: IssueHistoryRefreshMetadataInput) -> Result<Self, IssueHistoryError> {
        if !valid_commit_sha(&input.head_commit)
            || input.tagged_commits > input.commits_scanned
            || input.oversized_commits_skipped > input.tagged_commits
            || input.comparison_failures_skipped > input.tagged_commits
        {
            return Err(IssueHistoryError::InvalidInput);
        }
        Ok(Self {
            head_commit: input.head_commit,
            commits_scanned: input.commits_scanned,
            tagged_commits: input.tagged_commits,
            oversized_commits_skipped: input.oversized_commits_skipped,
            comparison_failures_skipped: input.comparison_failures_skipped,
            truncated: input.truncated,
        })
    }
}

/// Atomic replacement request for one current generation's issue evidence.
pub struct IssueHistoryRefreshRequest {
    project_id: ProjectId,
    generation_id: GenerationId,
    metadata: IssueHistoryRefreshMetadata,
    attributions: Vec<SymbolIssueAttribution>,
}

/// Bounded issue rows and provenance supplied by one Git scan.
pub struct IssueHistoryRefreshInput {
    /// Metadata for this record.
    pub metadata: IssueHistoryRefreshMetadata,
    /// Bounded attributions included in this result.
    pub attributions: Vec<SymbolIssueAttribution>,
}

impl IssueHistoryRefreshRequest {
    /// Validates and binds a refresh payload to one project generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the attribution payload exceeds the bounded atomic
    /// refresh maximum.
    pub fn new(
        project_id: ProjectId,
        generation_id: GenerationId,
        input: IssueHistoryRefreshInput,
    ) -> Result<Self, IssueHistoryError> {
        if input.attributions.len() > MAXIMUM_ATTRIBUTIONS {
            return Err(IssueHistoryError::InvalidInput);
        }
        Ok(Self {
            project_id,
            generation_id,
            metadata: input.metadata,
            attributions: input.attributions,
        })
    }
}

/// Durable issue-history replacement accounting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueHistoryRefreshReport {
    /// Stable generation ID for this record.
    pub generation_id: GenerationId,
    /// Head commit for this record.
    pub head_commit: String,
    /// Number of commits scanned.
    pub commits_scanned: u64,
    /// Number of tagged commits.
    pub tagged_commits: u64,
    /// Number of oversized commits skipped.
    pub oversized_commits_skipped: u64,
    /// Number of comparison failures skipped.
    pub comparison_failures_skipped: u64,
    /// Number of candidates.
    pub candidates: u64,
    /// Number of attributions written.
    pub attributions_written: u64,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
}

/// One current symbol/issue/commit attribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssueRecord {
    /// Positive issue number parsed from the tagged commit.
    pub issue_number: u64,
    /// Commit sha for this record.
    pub commit_sha: String,
    /// Kind for this record.
    pub kind: IssueAttributionKind,
}

/// A current symbol sharing issue-tagged commits with an anchor symbol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssuePeer {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Project-relative path for this record.
    pub path: String,
    /// Qualified name for this record.
    pub qualified_name: String,
    /// Issue-tagged commits shared with the anchor symbol.
    pub co_occurrences: u64,
    /// Bounded shared commits included in this result.
    pub shared_commits: Vec<String>,
}

/// One current symbol attributed to the same issue-tagged commit as an anchor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssueCommitPeer {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Project-relative path for this record.
    pub path: String,
    /// Qualified name for this record.
    pub qualified_name: String,
    /// Symbol kind for this record.
    pub symbol_kind: String,
}

/// Bounded current-generation peers for one exact commit SHA.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssueCommitPeers {
    /// Commit sha for this record.
    pub commit_sha: String,
    /// Total matching peers before the per-commit page bound.
    pub total: u64,
    /// Bounded peers included in this result.
    pub peers: Vec<SymbolIssueCommitPeer>,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
}

/// Bounded issue evidence query for one current symbol.
pub struct SymbolIssueQuery<'query> {
    /// Stable project ID for this record.
    pub project_id: &'query ProjectId,
    /// Stable symbol ID for this record.
    pub symbol_id: &'query SymbolId,
    /// Maximum number of entries returned by this bounded request.
    pub limit: u16,
}

/// Bounded co-attribution query for one current symbol.
pub struct SymbolIssuePeerQuery<'query> {
    /// Stable project ID for this record.
    pub project_id: &'query ProjectId,
    /// Stable symbol ID for this record.
    pub symbol_id: &'query SymbolId,
    /// Minimum number of shared required by this request.
    pub minimum_shared: u16,
    /// Maximum number of entries returned by this bounded request.
    pub limit: u16,
}

/// Bounded current-symbol query for exact issue-tagged commits.
pub struct IssueCommitSymbolPeerQuery<'query> {
    /// Stable project ID for this record.
    pub project_id: &'query ProjectId,
    /// Stable excluded symbol ID for this record.
    pub excluded_symbol_id: &'query SymbolId,
    /// Commits for this record.
    pub commits: &'query [String],
    /// Maximum peers returned for each supplied commit.
    pub per_commit_limit: u16,
}

/// Safe issue-history persistence/query failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum IssueHistoryError {
    #[error("issue-history input is invalid or exceeds its bound")]
    /// Supplied input violates a documented bound or invariant.
    InvalidInput,
    #[error("issue-history generation is no longer current")]
    /// The current generation changed before publication completed.
    CurrentGenerationChanged,
    #[error("Cartograph native storage issue-history operation failed during {operation}")]
    /// native storage could not complete the named operation.
    DatabaseOperation {
        /// Bounded operation label identifying the failed native storage phase.
        operation: &'static str,
    },
    #[error("Cartograph native storage issue-history row violates its durable contract")]
    /// A stored row violates its durable typed contract.
    CorruptStoredValue,
}

fn valid_issue_commit_peer_query(request: &IssueCommitSymbolPeerQuery<'_>) -> bool {
    if request.commits.is_empty() || request.commits.len() > MAXIMUM_COMMIT_GROUPS {
        return false;
    }
    if request.per_commit_limit == 0 || request.per_commit_limit > MAXIMUM_PEERS {
        return false;
    }
    request
        .commits
        .iter()
        .all(|commit| valid_commit_sha(commit))
        && request.commits.iter().collect::<BTreeSet<_>>().len() == request.commits.len()
}

fn valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn attribution_validation_accepts_sha1_and_sha256_but_rejects_zero_issue() {
        let symbol = SymbolId::parse("11111111-1111-4111-8111-111111111111")
            .unwrap_or_else(|error| panic!("symbol failed: {error}"));
        assert!(
            SymbolIssueAttribution::new(SymbolIssueAttributionInput {
                symbol_id: symbol.clone(),
                issue_number: 42,
                commit_sha: "a".repeat(40),
                kind: IssueAttributionKind::Modified,
            })
            .is_ok()
        );
        assert!(
            SymbolIssueAttribution::new(SymbolIssueAttributionInput {
                symbol_id: symbol.clone(),
                issue_number: 42,
                commit_sha: "b".repeat(64),
                kind: IssueAttributionKind::Added,
            })
            .is_ok()
        );
        assert_eq!(
            SymbolIssueAttribution::new(SymbolIssueAttributionInput {
                symbol_id: symbol,
                issue_number: 0,
                commit_sha: "c".repeat(40),
                kind: IssueAttributionKind::Removed,
            }),
            Err(IssueHistoryError::InvalidInput)
        );
    }

    #[test]
    fn refresh_request_rejects_skip_counts_larger_than_tagged_history() {
        assert_matches!(
            IssueHistoryRefreshMetadata::new(IssueHistoryRefreshMetadataInput {
                head_commit: "a".repeat(40),
                commits_scanned: 1,
                tagged_commits: 1,
                oversized_commits_skipped: 2,
                comparison_failures_skipped: 0,
                truncated: false,
            }),
            Err(IssueHistoryError::InvalidInput)
        );
    }
}
