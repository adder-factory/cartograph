use std::{collections::BTreeMap, fmt, path::Path, process::Stdio, time::Duration};

use cartograph_domain::NormalizedPath;
use cartograph_search::{
    DeterministicRetriever, IndexFreshness, ReviewBudget, ReviewPacket, ReviewRequest,
    ReviewRequestOptions,
};
use serde::Serialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::{Instant, timeout_at},
};

const DEFAULT_MAX_CHANGED_FILES: u16 = 200;
const MAX_CHANGED_FILES: u16 = 512;
const MAX_REF_BYTES: usize = 256;
const MAX_GIT_OUTPUT_BYTES: usize = 4 * 1_048_576;
const HARD_MAX_GIT_OUTPUT_BYTES: usize = 128 * 1_048_576;
const GIT_READ_CHUNK_BYTES: usize = 8 * 1_024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_REAP_TIMEOUT: Duration = Duration::from_secs(1);
const HARD_MAX_GIT_COMMAND_TIMEOUT: Duration = Duration::from_mins(2);

/// Bounded deterministic options for comparison against one Git revision.
#[derive(Clone, PartialEq, Eq)]
pub struct ReviewOptions {
    base_ref: String,
    max_changed_files: u16,
    evidence_budget: ReviewBudget,
    path_filter: Option<NormalizedPath>,
}

impl ReviewOptions {
    /// Validate a revision expression before it reaches Git.
    /// # Errors
    ///
    /// Returns [`ReviewError::InvalidRef`] when `base_ref` is empty,
    /// option-shaped, oversized, or contains whitespace or control bytes.
    pub fn new(base_ref: impl Into<String>) -> Result<Self, ReviewError> {
        let base_ref = base_ref.into();
        if !valid_git_ref(&base_ref) {
            return Err(ReviewError::InvalidRef);
        }
        Ok(Self {
            base_ref,
            max_changed_files: DEFAULT_MAX_CHANGED_FILES,
            evidence_budget: ReviewBudget::default(),
            path_filter: None,
        })
    }

    /// Bound the number of changed files retained in the response.
    /// # Errors
    ///
    /// Returns [`ReviewError::InvalidOptions`] when `limit` is zero or exceeds
    /// the retained changed-file ceiling.
    pub fn with_max_changed_files(mut self, limit: u16) -> Result<Self, ReviewError> {
        if limit == 0 || limit > MAX_CHANGED_FILES {
            return Err(ReviewError::InvalidOptions);
        }
        self.max_changed_files = limit;
        Ok(self)
    }

    /// Restrict changed paths to one canonical project-relative prefix.
    /// # Errors
    ///
    /// Returns [`ReviewError::InvalidOptions`] when the filter is empty,
    /// absolute, parent-escaping, contains NUL, or exceeds the path bound.
    pub fn with_path_filter(mut self, filter: Option<&str>) -> Result<Self, ReviewError> {
        self.path_filter = filter
            .map(NormalizedPath::parse)
            .transpose()
            .map_err(|_| ReviewError::InvalidOptions)?;
        Ok(self)
    }

    /// Validated Git revision expression.
    #[must_use]
    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// Maximum changed-file records retained after deterministic ordering.
    #[must_use]
    pub const fn max_changed_files(&self) -> u16 {
        self.max_changed_files
    }

    /// Override the validated deterministic evidence budget.
    #[must_use]
    pub fn with_evidence_budget(mut self, budget: ReviewBudget) -> Self {
        self.evidence_budget = budget;
        self
    }

    /// Active graph/evidence limits.
    #[must_use]
    pub const fn evidence_budget(&self) -> ReviewBudget {
        self.evidence_budget
    }
}

impl fmt::Debug for ReviewOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReviewOptions")
            .field("base_ref", &"<redacted>")
            .field("max_changed_files", &self.max_changed_files)
            .field("evidence_budget", &self.evidence_budget)
            .field(
                "path_filter",
                &self.path_filter.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Git-level classification for one changed project-relative path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeKind {
    /// Tracked path added relative to `HEAD`.
    Added,
    /// Tracked path modified relative to `HEAD`.
    Modified,
    /// Tracked path deleted relative to `HEAD`.
    Deleted,
    /// Tracked path changed filesystem type.
    TypeChanged,
    /// Path has unresolved merge state.
    Unmerged,
    /// Path is not tracked by Git.
    Untracked,
}

impl GitChangeKind {
    /// Stable human/wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::TypeChanged => "type_changed",
            Self::Unmerged => "unmerged",
            Self::Untracked => "untracked",
        }
    }
}

/// One deterministically ordered changed path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitChangedFile {
    path: NormalizedPath,
    kind: GitChangeKind,
}

impl GitChangedFile {
    const fn new(path: NormalizedPath, kind: GitChangeKind) -> Self {
        Self { path, kind }
    }

    /// Canonical path relative to the reviewed project root.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Git-level change classification.
    #[must_use]
    pub const fn kind(&self) -> GitChangeKind {
        self.kind
    }
}

/// Bounded Git comparison used as input to deterministic graph review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitComparison {
    base_ref: String,
    base_commit: String,
    files: Vec<GitChangedFile>,
    worktree_dirty: bool,
    truncated: bool,
}

/// Complete compare-to-ref report for CLI and MCP adapters.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReviewReport {
    comparison: GitComparison,
    packet: ReviewPacket,
}

impl ReviewReport {
    /// Bounded Git provenance and changed paths.
    #[must_use]
    pub const fn comparison(&self) -> &GitComparison {
        &self.comparison
    }

    /// Current-generation context, impact, tests, freshness, and abstention.
    #[must_use]
    pub const fn packet(&self) -> &ReviewPacket {
        &self.packet
    }
}

impl GitComparison {
    /// Validated user-facing base revision expression.
    #[must_use]
    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// Resolved immutable SHA-1 or SHA-256 commit identity.
    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    /// Changed files ordered by canonical path and status.
    #[must_use]
    pub fn files(&self) -> &[GitChangedFile] {
        &self.files
    }

    /// Whether staged, unstaged, or untracked work differs from `HEAD`.
    #[must_use]
    pub const fn worktree_dirty(&self) -> bool {
        self.worktree_dirty
    }

    /// Whether the changed-file response bound omitted a stable suffix.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Credential-, path-, and revision-safe review failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ReviewError {
    /// The revision is empty, option-shaped, contains whitespace/control bytes, or is too long.
    #[error("Git comparison ref is invalid")]
    InvalidRef,
    /// A configured result bound is zero or exceeds the hard ceiling.
    #[error("Git comparison options are invalid")]
    InvalidOptions,
    /// The selected project is not inside a Git worktree.
    #[error("Cartograph review requires a Git worktree")]
    NotGitRepository,
    /// Git could not resolve the validated revision to a commit.
    #[error("Git comparison ref was not found")]
    GitRefNotFound,
    /// Git could not be started in the current runtime environment.
    #[error("Git is unavailable for Cartograph review")]
    GitUnavailable,
    /// Git exceeded the hard execution deadline.
    #[error("Git comparison exceeded its deadline")]
    GitDeadlineExceeded,
    /// Git emitted more bytes than the hard transport bound.
    #[error("Git comparison output exceeded its byte limit")]
    GitOutputLimitExceeded,
    /// Git returned malformed, non-UTF-8, or unsafe project paths.
    #[error("Git comparison output is invalid")]
    GitOutputInvalid,
    /// A bounded Git operation failed without exposing command output or paths.
    #[error("Git comparison failed")]
    GitCommandFailed,
    /// The project root is missing, inaccessible, or not a directory.
    #[error("Cartograph review project root is unavailable")]
    ProjectRootUnavailable,
    /// Current project/generation state could not be read.
    #[error("Cartograph review project state is unavailable")]
    ProjectStateUnavailable,
    /// Deterministic index or graph retrieval failed.
    #[error("Cartograph review evidence is unavailable")]
    RetrievalUnavailable,
}

impl super::ProjectRuntime {
    /// Inspect Git dirtiness through the existing bounded asynchronous Git owner.
    /// An unavailable or oversized observation conservatively requests sync;
    /// only a successful empty status can prove that Git is clean.
    pub async fn git_worktree_is_dirty(&self) -> bool {
        git_worktree_is_dirty(&self.root).await
    }

    /// Compare the live checkout to a Git ref and attach deterministic current-generation
    /// exact-path, reverse-impact, and affected-test evidence.
    /// # Errors
    ///
    /// Returns an error when the checkout/ref or bounded Git output is invalid
    /// or unavailable, project freshness cannot be established, or deterministic
    /// review evidence cannot be retrieved.
    pub async fn review(&self, options: &ReviewOptions) -> Result<ReviewReport, ReviewError> {
        self.review_with_cancellation(options, super::ProjectCancellation::new())
            .await
    }

    /// Compare to a Git ref while keeping the source-freshness scan cancellable.
    /// # Errors
    ///
    /// Returns an error when the checkout/ref or bounded Git output is invalid
    /// or unavailable, the cancellable freshness scan fails, or deterministic
    /// review evidence cannot be retrieved.
    pub async fn review_with_cancellation(
        &self,
        options: &ReviewOptions,
        cancellation: super::ProjectCancellation,
    ) -> Result<ReviewReport, ReviewError> {
        let comparison = discover_git_comparison(&self.root, options).await?;
        let status = self
            .status_with_cancellation(cancellation)
            .await
            .map_err(|_| ReviewError::ProjectStateUnavailable)?;
        let (project_id, freshness) = match status.snapshot {
            Some(snapshot) if snapshot.current.is_some() => (
                Some(snapshot.project_id),
                if status.fresh {
                    IndexFreshness::Current
                } else {
                    IndexFreshness::Stale
                },
            ),
            _ => (None, IndexFreshness::Unknown),
        };
        let request = ReviewRequest::new(
            project_id,
            comparison.files.iter().map(|file| file.path.clone()),
            ReviewRequestOptions::new(freshness, options.evidence_budget)
                .with_changed_files_truncated(comparison.truncated),
        )
        .map_err(|_| ReviewError::RetrievalUnavailable)?;
        let packet = DeterministicRetriever::new(self.database.clone())
            .review_packet(&request)
            .await
            .map_err(|_| ReviewError::RetrievalUnavailable)?;
        Ok(ReviewReport { comparison, packet })
    }
}

/// Discover committed, staged, unstaged, and untracked changes without a shell.
/// # Errors
///
/// Returns an error when `project_root` is unavailable or not a Git worktree,
/// the validated base ref cannot be resolved, or a bounded Git subprocess
/// fails, times out, exceeds its output ceiling, or returns unsafe output.
pub async fn discover_git_comparison(
    project_root: impl AsRef<Path>,
    options: &ReviewOptions,
) -> Result<GitComparison, ReviewError> {
    let root = std::fs::canonicalize(project_root.as_ref())
        .map_err(|_| ReviewError::ProjectRootUnavailable)?;
    if !root.is_dir() {
        return Err(ReviewError::ProjectRootUnavailable);
    }

    let repository = run_git(&root, &["rev-parse", "--is-inside-work-tree"]).await?;
    if !repository.success || crate::trim_ascii_bytes(&repository.stdout) != b"true" {
        return Err(ReviewError::NotGitRepository);
    }

    let commit_expression = format!("{}^{{commit}}", options.base_ref);
    let resolved = run_git(
        &root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &commit_expression,
        ],
    )
    .await?;
    if !resolved.success {
        return Err(ReviewError::GitRefNotFound);
    }
    let base_commit = parse_commit(&resolved.stdout)?;

    let tracked = run_git(
        &root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--relative",
            &base_commit,
            "--",
            ".",
            ":(exclude).cartograph",
        ],
    )
    .await?;
    if !tracked.success {
        return Err(ReviewError::GitCommandFailed);
    }
    let untracked = run_git(
        &root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
            ":(exclude).cartograph",
        ],
    )
    .await?;
    if !untracked.success {
        return Err(ReviewError::GitCommandFailed);
    }
    let status = run_git(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--",
            ".",
            ":(exclude).cartograph",
        ],
    )
    .await?;
    if !status.success {
        return Err(ReviewError::GitCommandFailed);
    }

    let mut changed = parse_name_status(&tracked.stdout)?;
    for path in parse_paths(&untracked.stdout)? {
        changed.entry(path).or_insert(GitChangeKind::Untracked);
    }
    if let Some(filter) = options.path_filter.as_ref() {
        changed.retain(|path, _| path_matches_filter(path, filter));
    }
    let truncated = changed.len() > usize::from(options.max_changed_files);
    let files = changed
        .into_iter()
        .take(usize::from(options.max_changed_files))
        .map(|(path, kind)| GitChangedFile::new(path, kind))
        .collect();
    Ok(GitComparison {
        base_ref: options.base_ref.clone(),
        base_commit,
        files,
        worktree_dirty: !status.stdout.is_empty(),
        truncated,
    })
}

pub(crate) fn path_matches_filter(path: &NormalizedPath, filter: &NormalizedPath) -> bool {
    path == filter
        || path
            .as_str()
            .strip_prefix(filter.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(super) struct GitOutput {
    pub(super) success: bool,
    pub(super) stdout: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(super) struct GitCommandBounds {
    maximum_output_bytes: usize,
    command_timeout: Duration,
}

impl GitCommandBounds {
    pub(super) const fn new(maximum_output_bytes: usize, command_timeout: Duration) -> Self {
        Self {
            maximum_output_bytes,
            command_timeout,
        }
    }
}

pub(super) async fn run_git(root: &Path, arguments: &[&str]) -> Result<GitOutput, ReviewError> {
    run_git_bounded(
        root,
        arguments,
        GitCommandBounds::new(MAX_GIT_OUTPUT_BYTES, GIT_COMMAND_TIMEOUT),
    )
    .await
}

pub(super) async fn run_git_bounded(
    root: &Path,
    arguments: &[&str],
    bounds: GitCommandBounds,
) -> Result<GitOutput, ReviewError> {
    let GitCommandBounds {
        maximum_output_bytes,
        command_timeout,
    } = bounds;
    if maximum_output_bytes == 0
        || maximum_output_bytes > HARD_MAX_GIT_OUTPUT_BYTES
        || command_timeout.is_zero()
        || command_timeout > HARD_MAX_GIT_COMMAND_TIMEOUT
    {
        return Err(ReviewError::InvalidOptions);
    }
    let mut child = Command::new("git")
        .arg("--no-pager")
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "diff.external="])
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .env_remove("GIT_EXTERNAL_DIFF")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ReviewError::GitUnavailable)?;
    let stdout = child.stdout.take().ok_or(ReviewError::GitUnavailable)?;
    let deadline = Instant::now() + command_timeout;
    let stdout = match timeout_at(deadline, read_bounded(stdout, maximum_output_bytes)).await {
        Ok(Ok(stdout)) => stdout,
        Ok(Err(error)) => {
            terminate(&mut child).await;
            return Err(error);
        }
        Err(_) => {
            terminate(&mut child).await;
            return Err(ReviewError::GitDeadlineExceeded);
        }
    };
    let status = match timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => return Err(ReviewError::GitCommandFailed),
        Err(_) => {
            terminate(&mut child).await;
            return Err(ReviewError::GitDeadlineExceeded);
        }
    };
    Ok(GitOutput {
        success: status.success(),
        stdout,
    })
}

async fn read_bounded(
    mut stdout: impl AsyncRead + Unpin,
    maximum_bytes: usize,
) -> Result<Vec<u8>, ReviewError> {
    let mut output = Vec::with_capacity(maximum_bytes.min(GIT_READ_CHUNK_BYTES));
    let mut chunk = [0_u8; GIT_READ_CHUNK_BYTES];
    loop {
        let count = stdout
            .read(&mut chunk)
            .await
            .map_err(|_| ReviewError::GitCommandFailed)?;
        if count == 0 {
            return Ok(output);
        }
        let next = output
            .len()
            .checked_add(count)
            .ok_or(ReviewError::GitOutputLimitExceeded)?;
        if next > maximum_bytes {
            return Err(ReviewError::GitOutputLimitExceeded);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

async fn terminate(child: &mut Child) {
    let _kill_result = child.start_kill();
    let _wait_result = tokio::time::timeout(GIT_REAP_TIMEOUT, child.wait()).await;
}

async fn git_worktree_is_dirty(root: &Path) -> bool {
    run_git_bounded(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--",
            ".",
            ":(exclude).cartograph",
        ],
        GitCommandBounds::new(1, Duration::from_secs(2)),
    )
    .await
    .map_or(true, |output| !output.success || !output.stdout.is_empty())
}

fn parse_commit(output: &[u8]) -> Result<String, ReviewError> {
    let text = std::str::from_utf8(crate::trim_ascii_bytes(output))
        .map_err(|_| ReviewError::GitOutputInvalid)?;
    let valid = matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(text.to_ascii_lowercase())
    } else {
        Err(ReviewError::GitOutputInvalid)
    }
}

fn parse_name_status(
    output: &[u8],
) -> Result<BTreeMap<NormalizedPath, GitChangeKind>, ReviewError> {
    let tokens = split_nul(output)?;
    let mut changed = BTreeMap::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let (status, path) = if let Some(separator) = token.iter().position(|byte| *byte == b'\t') {
            (&token[..separator], &token[separator + 1..])
        } else {
            let path = tokens
                .get(index + 1)
                .copied()
                .ok_or(ReviewError::GitOutputInvalid)?;
            index += 1;
            (token, path)
        };
        let kind = parse_change_kind(status)?;
        changed.insert(parse_path(path)?, kind);
        index += 1;
    }
    Ok(changed)
}

fn parse_paths(output: &[u8]) -> Result<Vec<NormalizedPath>, ReviewError> {
    split_nul(output)?.into_iter().map(parse_path).collect()
}

fn split_nul(output: &[u8]) -> Result<Vec<&[u8]>, ReviewError> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if output.last() != Some(&0) {
        return Err(ReviewError::GitOutputInvalid);
    }
    Ok(output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .collect())
}

fn parse_change_kind(status: &[u8]) -> Result<GitChangeKind, ReviewError> {
    match status.first().copied() {
        Some(b'A') => Ok(GitChangeKind::Added),
        Some(b'M') => Ok(GitChangeKind::Modified),
        Some(b'D') => Ok(GitChangeKind::Deleted),
        Some(b'T') => Ok(GitChangeKind::TypeChanged),
        Some(b'U') => Ok(GitChangeKind::Unmerged),
        _ => Err(ReviewError::GitOutputInvalid),
    }
}

fn parse_path(raw: &[u8]) -> Result<NormalizedPath, ReviewError> {
    let path = std::str::from_utf8(raw).map_err(|_| ReviewError::GitOutputInvalid)?;
    if path.chars().any(char::is_control) {
        return Err(ReviewError::GitOutputInvalid);
    }
    NormalizedPath::parse(path).map_err(|_| ReviewError::GitOutputInvalid)
}

pub(super) fn valid_git_ref(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= MAX_REF_BYTES
        && !reference.starts_with('-')
        && !reference
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
}

#[cfg(test)]
mod dirtiness_tests {
    use super::*;

    #[tokio::test]
    async fn git_dirtiness_requires_a_successful_empty_observation() {
        let project =
            tempfile::tempdir().unwrap_or_else(|error| panic!("Git fixture failed: {error}"));
        assert!(
            git_worktree_is_dirty(project.path()).await,
            "an unavailable Git repository is not clean evidence"
        );
        // Keep host Git templates out of this bounded dirtiness fixture.
        let initialized = run_git_bounded(
            project.path(),
            &["init", "--initial-branch=main", "--template="],
            GitCommandBounds::new(1_024, Duration::from_secs(2)),
        )
        .await
        .unwrap_or_else(|error| panic!("Git init failed: {error}"));
        assert!(initialized.success);
        assert!(!git_worktree_is_dirty(project.path()).await);
        std::fs::write(project.path().join("untracked.rs"), "pub fn value() {}\n")
            .unwrap_or_else(|error| panic!("Git source failed: {error}"));
        assert!(
            git_worktree_is_dirty(project.path()).await,
            "oversized status must still request sync"
        );
    }
}
