use std::{
    collections::{BTreeSet, HashSet},
    sync::OnceLock,
    time::Duration,
};

use cartograph_db::{
    IssueAttributionKind, IssueHistoryRefreshInput, IssueHistoryRefreshMetadata,
    IssueHistoryRefreshMetadataInput, IssueHistoryRefreshReport, IssueHistoryRefreshRequest,
    SymbolIssueAttribution, SymbolIssueAttributionInput,
};
use cartograph_domain::{GenerationId, ProjectId};
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use regex::Regex;
use thiserror::Error;

use crate::{
    ProjectCancellation, ProjectRuntime, SourceCompareError, SourceCompareOptions,
    SourceCompareReport,
    review::{GitCommandBounds, run_git_bounded},
};

const DEFAULT_MAXIMUM_COMMITS: u32 = 20_000;
const MAXIMUM_COMMITS: u32 = 50_000;
const MAXIMUM_TAGGED_COMMITS: usize = 2_000;
const MAXIMUM_FILES_PER_TAGGED_COMMIT: u16 = 50;
const COMPARE_ADMISSION_LIMIT: u16 = MAXIMUM_FILES_PER_TAGGED_COMMIT + 1;
const COMPARE_CONCURRENCY: usize = 8;
const MAXIMUM_GIT_LOG_BYTES: usize = 128 * 1024 * 1024;
const GIT_LOG_TIMEOUT: Duration = Duration::from_mins(2);
const MAXIMUM_ATTRIBUTIONS: usize = 500_000;
const ISSUE_RECORD_MARKER: &str = "CARTOGRAPH_ISSUE";

/// Bounded issue-tagged history scan options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IssueHistoryIndexOptions {
    maximum_commits: u32,
}

impl Default for IssueHistoryIndexOptions {
    fn default() -> Self {
        Self {
            maximum_commits: DEFAULT_MAXIMUM_COMMITS,
        }
    }
}

impl IssueHistoryIndexOptions {
    /// Sets the maximum commits and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns [`IssueHistoryIndexError::InvalidOptions`] when
    /// `maximum_commits` is zero or exceeds the issue-history ceiling.
    pub const fn with_maximum_commits(
        mut self,
        maximum_commits: u32,
    ) -> Result<Self, IssueHistoryIndexError> {
        if maximum_commits == 0 || maximum_commits > MAXIMUM_COMMITS {
            return Err(IssueHistoryIndexError::InvalidOptions);
        }
        self.maximum_commits = maximum_commits;
        Ok(self)
    }
}

/// Exact generation, scan bounds, and cancellation scope for issue-history refresh.
pub struct IssueHistoryIndexRequest {
    /// Stable project ID for this record.
    pub project_id: ProjectId,
    /// Stable generation ID for this record.
    pub generation_id: GenerationId,
    /// Options for this record.
    pub options: IssueHistoryIndexOptions,
    /// Cancellation for this record.
    pub cancellation: ProjectCancellation,
}

/// Source-safe issue-history failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum IssueHistoryIndexError {
    /// The project's explicit issue-history policy disables this derived channel.
    #[error("issue history is disabled by project configuration")]
    DisabledByProjectConfig,
    #[error("issue-history options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("issue-history requires a Git repository with HEAD")]
    /// The project root is not inside the required Git repository.
    NotGitRepository,
    #[error("issue-history Git evidence is unavailable or malformed")]
    /// The bounded Git subprocess could not be executed.
    GitUnavailable,
    #[error("issue-history source comparison failed")]
    /// Git could not compare the requested revisions completely.
    ComparisonFailed,
    #[error("issue-history exceeded its deterministic relation bound")]
    /// Derived relationships exceeded their declared ceiling.
    RelationLimit,
    #[error("issue-history indexing was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
    #[error("issue-history persistence failed")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
}

impl ProjectRuntime {
    /// Rebuild issue-tagged symbol evidence for one exact current generation.
    /// # Errors
    ///
    /// Returns an error when the checkout lacks readable bounded Git history,
    /// an issue commit cannot be compared completely, attribution limits are
    /// exceeded, cancellation wins, or generation-fenced persistence fails.
    pub async fn refresh_issue_history(
        &self,
        input: IssueHistoryIndexRequest,
    ) -> Result<IssueHistoryRefreshReport, IssueHistoryIndexError> {
        let IssueHistoryIndexRequest {
            project_id,
            generation_id,
            options,
            cancellation,
        } = input;
        let prepared = self.prepare_issue_history(options, cancellation).await?;
        self.persist_issue_history(project_id, generation_id, prepared)
            .await
    }

    pub(crate) async fn prepare_issue_history(
        &self,
        options: IssueHistoryIndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<PreparedIssueHistory, IssueHistoryIndexError> {
        if cancellation.is_cancelled() {
            return Err(IssueHistoryIndexError::Cancelled);
        }
        let head = git_head(self).await?;
        let scan = load_issue_commits(self, options.maximum_commits).await?;
        let tagged_commits = u64::try_from(scan.commits.len()).unwrap_or(u64::MAX);
        let root = self.project_root_for_host_operations();
        let comparisons = compare_issue_commits(root, scan.commits, cancellation).await?;
        let collected = collect_issue_attributions(comparisons)?;
        Ok(PreparedIssueHistory {
            head,
            commits_scanned: scan.commits_scanned,
            tagged_commits,
            truncated: scan.truncated,
            oversized_commits_skipped: collected.oversized_commits_skipped,
            comparison_failures_skipped: collected.comparison_failures_skipped,
            attributions: collected.attributions,
        })
    }

    pub(crate) async fn persist_issue_history(
        &self,
        project_id: ProjectId,
        generation_id: GenerationId,
        prepared: PreparedIssueHistory,
    ) -> Result<IssueHistoryRefreshReport, IssueHistoryIndexError> {
        let metadata = IssueHistoryRefreshMetadata::new(IssueHistoryRefreshMetadataInput {
            head_commit: prepared.head,
            commits_scanned: prepared.commits_scanned,
            tagged_commits: prepared.tagged_commits,
            oversized_commits_skipped: prepared.oversized_commits_skipped,
            comparison_failures_skipped: prepared.comparison_failures_skipped,
            truncated: prepared.truncated,
        })
        .map_err(|_| IssueHistoryIndexError::RelationLimit)?;
        let request = IssueHistoryRefreshRequest::new(
            project_id,
            generation_id,
            IssueHistoryRefreshInput {
                metadata,
                attributions: prepared.attributions,
            },
        )
        .map_err(|_| IssueHistoryIndexError::RelationLimit)?;
        self.database()
            .replace_issue_history(request)
            .await
            .map_err(|_| IssueHistoryIndexError::StorageUnavailable)
    }
}

type IssueComparison = (usize, IssueCommit, Option<SourceCompareReport>);

async fn compare_issue_commits(
    root: &std::path::Path,
    commits: Vec<IssueCommit>,
    cancellation: ProjectCancellation,
) -> Result<Vec<IssueComparison>, IssueHistoryIndexError> {
    let comparisons = stream::iter(commits.into_iter().enumerate())
        .map(|(index, commit)| {
            let cancellation = cancellation.clone();
            async move {
                if cancellation.is_cancelled() {
                    return Err(IssueHistoryIndexError::Cancelled);
                }
                let base = format!("{}^", commit.sha);
                let options = SourceCompareOptions::new(base)
                    .and_then(|options| options.with_head(&commit.sha))
                    .and_then(|options| options.with_max_changed_files(COMPARE_ADMISSION_LIMIT))
                    .map_err(|_| IssueHistoryIndexError::ComparisonFailed)?;
                let comparison =
                    crate::compare::compare_sources_at(root, options, cancellation).await;
                match comparison {
                    Ok(comparison) => Ok(Some((index, commit, Some(comparison)))),
                    Err(SourceCompareError::RefNotFound) => Ok(None),
                    Err(SourceCompareError::Cancelled) => Err(IssueHistoryIndexError::Cancelled),
                    Err(_) => Ok(Some((index, commit, None))),
                }
            }
        })
        .buffer_unordered(COMPARE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    let mut comparisons = comparisons.into_iter().flatten().collect::<Vec<_>>();
    comparisons.sort_by_key(|(index, _, _)| *index);
    Ok(comparisons)
}

struct CollectedIssueAttributions {
    oversized_commits_skipped: u64,
    comparison_failures_skipped: u64,
    attributions: Vec<SymbolIssueAttribution>,
}

fn collect_issue_attributions(
    comparisons: Vec<IssueComparison>,
) -> Result<CollectedIssueAttributions, IssueHistoryIndexError> {
    let mut attributions = BTreeSet::new();
    let mut oversized_commits_skipped = 0_u64;
    let mut comparison_failures_skipped = 0_u64;
    for (_, commit, comparison) in comparisons {
        let Some(comparison) = comparison else {
            comparison_failures_skipped = comparison_failures_skipped.saturating_add(1);
            continue;
        };
        if comparison.exceeds_changed_file_limit(u64::from(MAXIMUM_FILES_PER_TAGGED_COMMIT)) {
            oversized_commits_skipped = oversized_commits_skipped.saturating_add(1);
            continue;
        }
        append_issue_attributions(&mut attributions, &commit, &comparison)?;
    }
    Ok(CollectedIssueAttributions {
        oversized_commits_skipped,
        comparison_failures_skipped,
        attributions: build_issue_attributions(attributions)?,
    })
}

fn append_issue_attributions(
    attributions: &mut BTreeSet<(
        cartograph_domain::SymbolId,
        u64,
        String,
        IssueAttributionKind,
    )>,
    commit: &IssueCommit,
    comparison: &SourceCompareReport,
) -> Result<(), IssueHistoryIndexError> {
    for (symbol_id, added) in comparison.current_issue_symbols() {
        let kind = if added {
            IssueAttributionKind::Added
        } else {
            IssueAttributionKind::Modified
        };
        for issue_number in &commit.issues {
            attributions.insert((symbol_id.clone(), *issue_number, commit.sha.clone(), kind));
            if attributions.len() > MAXIMUM_ATTRIBUTIONS {
                return Err(IssueHistoryIndexError::RelationLimit);
            }
        }
    }
    Ok(())
}

fn build_issue_attributions(
    attributions: BTreeSet<(
        cartograph_domain::SymbolId,
        u64,
        String,
        IssueAttributionKind,
    )>,
) -> Result<Vec<SymbolIssueAttribution>, IssueHistoryIndexError> {
    attributions
        .into_iter()
        .map(|(symbol_id, issue_number, sha, kind)| {
            SymbolIssueAttribution::new(SymbolIssueAttributionInput {
                symbol_id,
                issue_number,
                commit_sha: sha,
                kind,
            })
            .map_err(|_| IssueHistoryIndexError::RelationLimit)
        })
        .collect()
}

async fn load_issue_commits(
    runtime: &ProjectRuntime,
    maximum_commits: u32,
) -> Result<IssueCommitScan, IssueHistoryIndexError> {
    let requested = maximum_commits
        .checked_add(1)
        .ok_or(IssueHistoryIndexError::InvalidOptions)?
        .to_string();
    let output = run_git_bounded(
        runtime.project_root_for_host_operations(),
        &[
            "log",
            "--no-merges",
            "-z",
            "--format=CARTOGRAPH_ISSUE%n%H%n%B",
            "--max-count",
            &requested,
            "HEAD",
            "--",
        ],
        GitCommandBounds::new(MAXIMUM_GIT_LOG_BYTES, GIT_LOG_TIMEOUT),
    )
    .await
    .map_err(|_| IssueHistoryIndexError::GitUnavailable)?;
    if !output.success {
        return Err(IssueHistoryIndexError::GitUnavailable);
    }
    parse_issue_commits(&output.stdout, maximum_commits)
}

pub(crate) struct PreparedIssueHistory {
    head: String,
    commits_scanned: u64,
    tagged_commits: u64,
    truncated: bool,
    oversized_commits_skipped: u64,
    comparison_failures_skipped: u64,
    attributions: Vec<SymbolIssueAttribution>,
}

struct IssueCommitScan {
    commits: Vec<IssueCommit>,
    commits_scanned: u64,
    truncated: bool,
}

struct IssueCommit {
    sha: String,
    issues: Vec<u64>,
}

fn parse_issue_commits(
    output: &[u8],
    maximum_commits: u32,
) -> Result<IssueCommitScan, IssueHistoryIndexError> {
    if maximum_commits == 0 || maximum_commits > MAXIMUM_COMMITS {
        return Err(IssueHistoryIndexError::InvalidOptions);
    }
    let source = std::str::from_utf8(output).map_err(|_| IssueHistoryIndexError::GitUnavailable)?;
    let regex = issue_regex()?;
    let mut commits = Vec::new();
    let mut commits_scanned = 0_u64;
    let mut truncated = false;
    for block in source.split('\0').filter(|block| !block.trim().is_empty()) {
        if commits_scanned >= u64::from(maximum_commits) {
            truncated = true;
            break;
        }
        let commit = parse_issue_commit(block, regex)?;
        commits_scanned = commits_scanned.saturating_add(1);
        if commit.issues.is_empty() {
            continue;
        }
        if commits.len() >= MAXIMUM_TAGGED_COMMITS {
            truncated = true;
            continue;
        }
        commits.push(commit);
    }
    Ok(IssueCommitScan {
        commits,
        commits_scanned,
        truncated,
    })
}

fn parse_issue_commit(block: &str, regex: &Regex) -> Result<IssueCommit, IssueHistoryIndexError> {
    let mut lines = block.trim().lines();
    if lines.next() != Some(ISSUE_RECORD_MARKER) {
        return Err(IssueHistoryIndexError::GitUnavailable);
    }
    let sha = lines
        .next()
        .filter(|sha| valid_sha(sha))
        .ok_or(IssueHistoryIndexError::GitUnavailable)?
        .to_owned();
    let message = lines.collect::<Vec<_>>().join("\n");
    Ok(IssueCommit {
        sha,
        issues: issue_numbers(regex, &message),
    })
}

fn issue_numbers(regex: &Regex, message: &str) -> Vec<u64> {
    let mut seen = HashSet::new();
    regex
        .captures_iter(message)
        .filter_map(|capture| {
            capture
                .get(1)
                .and_then(|value| value.as_str().parse::<u64>().ok())
                .filter(|value| *value > 0 && *value <= 9_223_372_036_854_775_807_u64)
        })
        .filter(|issue| seen.insert(*issue))
        .collect()
}

async fn git_head(runtime: &ProjectRuntime) -> Result<String, IssueHistoryIndexError> {
    let output = run_git_bounded(
        runtime.project_root_for_host_operations(),
        &["rev-parse", "--verify", "HEAD"],
        GitCommandBounds::new(256, Duration::from_secs(10)),
    )
    .await
    .map_err(|_| IssueHistoryIndexError::NotGitRepository)?;
    let head = std::str::from_utf8(&output.stdout)
        .ok()
        .map(str::trim)
        .filter(|head| output.success && valid_sha(head))
        .ok_or(IssueHistoryIndexError::NotGitRepository)?;
    Ok(head.to_owned())
}

fn issue_regex() -> Result<&'static Regex, IssueHistoryIndexError> {
    static REGEX: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(
                r"(?i)\b(?:fix|fixes|fixed|close|closes|closed|resolve|resolves|resolved)\s*[:-]?\s*#([0-9]+)",
            )
        })
        .as_ref()
        .map_err(|_| IssueHistoryIndexError::GitUnavailable)
}

fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_log_parser_supports_multiple_verbs_deduplicates_and_bounds_commits() {
        let output = concat!(
            "CARTOGRAPH_ISSUE\n",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            "Fixes #12, closes: #13 and fixes #12\0",
            "CARTOGRAPH_ISSUE\n",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
            "ordinary maintenance\0",
            "CARTOGRAPH_ISSUE\n",
            "cccccccccccccccccccccccccccccccccccccccc\n",
            "Resolves #14\0"
        );
        let scan = parse_issue_commits(output.as_bytes(), 2)
            .unwrap_or_else(|error| panic!("issue parse failed: {error}"));
        assert_eq!(scan.commits_scanned, 2);
        assert!(scan.truncated);
        assert_eq!(scan.commits.len(), 1);
        assert_eq!(scan.commits[0].issues, [12, 13]);
    }
}
