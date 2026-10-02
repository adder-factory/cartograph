use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use cartograph_domain::{NormalizedPath, SymbolId};
use cartograph_extract::{
    ExtractedFile, ExtractedSymbol, NativeExtractor, SourceLimits, SourceSnapshot,
    SymbolHealthMetrics,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use thiserror::Error;

use crate::{
    ProjectCancellation, ProjectRuntime,
    review::{GitCommandBounds, run_git, run_git_bounded, valid_git_ref},
    source_limits,
};

const DEFAULT_MAX_CHANGED_FILES: u16 = 200;
const MAX_CHANGED_FILES: u16 = 512;
const MAX_PATH_FILTER_BYTES: usize = 4_096;
const COMPARE_CONCURRENCY: usize = 8;
const GIT_SOURCE_OUTPUT_BYTES: usize = 64 * 1_024 * 1_024;
const GIT_SOURCE_TIMEOUT: Duration = Duration::from_secs(30);
const LARGE_METHOD_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(100.0, 200.0);
const COMPLEX_METHOD_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(15.0, 25.0);
const NESTED_COMPLEXITY_THRESHOLDS: CompareFindingThreshold =
    CompareFindingThreshold::new(5.0, 7.0);
const COMPLEX_CONDITIONAL_THRESHOLDS: CompareFindingThreshold =
    CompareFindingThreshold::new(6.0, 8.0);
const LONG_PARAMETER_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(5.0, 7.0);
const ONE_TO_ONE_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(1.0, 1.0);
const ONE_TO_THREE_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(1.0, 3.0);
const ONE_TO_FIVE_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(1.0, 5.0);
const TWO_TO_FIVE_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(2.0, 5.0);
const THREE_TO_FIVE_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(3.0, 5.0);
const FIVE_TO_FIFTY_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(5.0, 50.0);
const EMPTY_BODY_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(1.0, f64::MAX);
const BRAIN_METHOD_THRESHOLDS: CompareFindingThreshold = CompareFindingThreshold::new(4.0, 6.0);
const BRAIN_CYCLOMATIC_NORMALIZER: f64 = 15.0;
const BRAIN_NESTING_NORMALIZER: f64 = 5.0;

#[derive(Clone, Copy)]
struct CompareFindingThreshold {
    warning: f64,
    error: f64,
}

impl CompareFindingThreshold {
    const fn new(warning: f64, error: f64) -> Self {
        Self { warning, error }
    }
}

#[derive(Clone, Copy)]
struct SourceFindingInput<'symbol> {
    symbol: &'symbol ExtractedSymbol,
    finding: &'static str,
    metric: f64,
    thresholds: CompareFindingThreshold,
}

/// Validated structural comparison policy for a worktree or two Git refs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCompareOptions {
    base_ref: String,
    head_ref: Option<String>,
    path_filter: Option<String>,
    max_changed_files: u16,
    findings: SourceFindingPolicy,
    delta: SourceDeltaPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceFindingPolicy {
    include: bool,
    delta: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceDeltaPolicy {
    edges: bool,
    suppress_line_range_only: bool,
}

impl SourceCompareOptions {
    /// Compare the worktree against one safe, resolvable base ref.
    /// # Errors
    ///
    /// Returns [`SourceCompareError::InvalidOptions`] when `base_ref` is not a
    /// bounded, syntactically safe Git reference.
    pub fn new(base_ref: impl Into<String>) -> Result<Self, SourceCompareError> {
        let base_ref = base_ref.into();
        if !valid_git_ref(&base_ref) {
            return Err(SourceCompareError::InvalidOptions);
        }
        Ok(Self {
            base_ref,
            head_ref: None,
            path_filter: None,
            max_changed_files: DEFAULT_MAX_CHANGED_FILES,
            findings: SourceFindingPolicy {
                include: false,
                delta: false,
            },
            delta: SourceDeltaPolicy {
                edges: false,
                suppress_line_range_only: true,
            },
        })
    }

    /// Switch to direct base-to-head ref comparison with two-dot semantics.
    /// # Errors
    ///
    /// Returns [`SourceCompareError::InvalidOptions`] when `head_ref` is not a
    /// bounded, syntactically safe Git reference.
    pub fn with_head(mut self, head_ref: impl Into<String>) -> Result<Self, SourceCompareError> {
        let head_ref = head_ref.into();
        if !valid_git_ref(&head_ref) {
            return Err(SourceCompareError::InvalidOptions);
        }
        self.head_ref = Some(head_ref);
        Ok(self)
    }

    /// Restrict changed paths to one canonical project-relative prefix.
    /// # Errors
    ///
    /// Returns [`SourceCompareError::InvalidOptions`] when the filter is empty,
    /// oversized, absolute, contains NUL, or contains a parent-directory segment.
    pub fn with_path_filter(
        mut self,
        path_filter: Option<&str>,
    ) -> Result<Self, SourceCompareError> {
        self.path_filter = path_filter
            .map(|filter| {
                if filter.is_empty()
                    || filter.len() > MAX_PATH_FILTER_BYTES
                    || filter.contains('\0')
                    || filter.starts_with('/')
                    || filter.split('/').any(|segment| segment == "..")
                {
                    return Err(SourceCompareError::InvalidOptions);
                }
                Ok(filter.trim_start_matches("./").to_owned())
            })
            .transpose()?;
        Ok(self)
    }

    /// Bound structurally analyzed paths while preserving the complete changed count.
    /// # Errors
    ///
    /// Returns [`SourceCompareError::InvalidOptions`] when `limit` is zero or
    /// exceeds the maximum structurally analyzed file count.
    pub fn with_max_changed_files(mut self, limit: u16) -> Result<Self, SourceCompareError> {
        if limit == 0 || limit > MAX_CHANGED_FILES {
            return Err(SourceCompareError::InvalidOptions);
        }
        self.max_changed_files = limit;
        Ok(self)
    }

    /// Attach in-memory per-file findings to added and modified symbols.
    #[must_use]
    pub fn with_findings(mut self, include: bool) -> Self {
        self.findings.include = include;
        self
    }

    /// Compute introduced, cleared, and carried per-file findings.
    #[must_use]
    pub fn with_findings_delta(mut self, include: bool) -> Self {
        self.findings.delta = include;
        self
    }

    /// Compute source-local structural reference and containment edge deltas.
    #[must_use]
    pub fn with_edges(mut self, include: bool) -> Self {
        self.delta.edges = include;
        self
    }

    /// Collapse modifications caused only by line-number movement.
    #[must_use]
    pub fn with_line_range_suppression(mut self, suppress: bool) -> Self {
        self.delta.suppress_line_range_only = suppress;
        self
    }
}

/// One source declaration added, removed, or structurally modified.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSymbolDelta {
    symbol_id: SymbolId,
    qualified_name: String,
    name: String,
    symbol_kind: String,
    start_line: u32,
    end_line: u32,
    modified_reasons: Vec<&'static str>,
    findings: Vec<SourceFinding>,
}

/// One deterministic per-file health finding calculated from an immutable blob.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFinding {
    qualified_name: String,
    finding: &'static str,
    severity: &'static str,
    metric: f64,
}

/// Added/removed source-local structural relation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceEdgeDelta {
    source: String,
    target: String,
    edge_kind: String,
}

/// Findings introduced, cleared, or retained by one file change.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFindingsDelta {
    introduced: Vec<SourceFinding>,
    cleared: Vec<SourceFinding>,
    carried: Vec<SourceFinding>,
}

/// Full structural delta for one path, including explicit unsupported/parse states.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFileDelta {
    path: NormalizedPath,
    had_baseline: bool,
    had_current: bool,
    language: Option<String>,
    added: Vec<SourceSymbolDelta>,
    removed: Vec<SourceSymbolDelta>,
    modified: Vec<SourceSymbolDelta>,
    line_range_only_count: u64,
    findings_delta: Option<SourceFindingsDelta>,
    edges_added: Vec<SourceEdgeDelta>,
    edges_removed: Vec<SourceEdgeDelta>,
    skip_reason: Option<&'static str>,
}

/// Rust-native structural comparison of the worktree or two immutable refs.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCompareReport {
    comparison: String,
    base_ref: String,
    base_commit: String,
    head_ref: Option<String>,
    head_commit: Option<String>,
    compares_worktree: bool,
    path_filter: Option<String>,
    changed_before_filter: u64,
    changed_after_filter: u64,
    files_analyzed: u64,
    files_skipped: u64,
    changed_files_truncated: bool,
    symbols_added: u64,
    symbols_removed: u64,
    symbols_modified: u64,
    edges_added: u64,
    edges_removed: u64,
    findings_introduced: u64,
    findings_cleared: u64,
    files: Vec<SourceFileDelta>,
}

impl SourceCompareReport {
    pub(crate) fn current_issue_symbols(&self) -> Vec<(SymbolId, bool)> {
        let mut symbols = BTreeMap::<SymbolId, bool>::new();
        for file in &self.files {
            for symbol in &file.modified {
                symbols.entry(symbol.symbol_id.clone()).or_insert(false);
            }
            for symbol in &file.added {
                symbols.insert(symbol.symbol_id.clone(), true);
            }
        }
        symbols.into_iter().collect()
    }

    pub(crate) fn exceeds_changed_file_limit(&self, maximum: u64) -> bool {
        self.changed_after_filter > maximum || self.changed_files_truncated
    }
}

/// Credential-, source-, and ref-safe structural comparison failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SourceCompareError {
    #[error("source comparison options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("source comparison requires a Git worktree")]
    /// The project root is not inside the required Git repository.
    NotGitRepository,
    #[error("one or more source comparison refs were not found")]
    /// Git could not resolve the requested comparison reference.
    RefNotFound,
    #[error("Git source comparison is unavailable")]
    /// The bounded Git subprocess could not be executed.
    GitUnavailable,
    #[error("source comparison output is invalid or exceeds its bound")]
    /// A subprocess returned output that violates its bounded schema.
    InvalidOutput,
    #[error("source comparison was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
}

impl ProjectRuntime {
    /// Compare source structure directly from immutable Git blobs or the live
    /// worktree. The persisted index is deliberately not used for the delta,
    /// so stale generations cannot poison end-of-task review.
    /// # Errors
    ///
    /// Returns an error when the project is not a readable Git worktree, a ref
    /// cannot be resolved, bounded Git output is unavailable or invalid, or the
    /// caller cancels the comparison.
    pub async fn compare_sources(
        &self,
        options: SourceCompareOptions,
        cancellation: ProjectCancellation,
    ) -> Result<SourceCompareReport, SourceCompareError> {
        compare_sources_at(&self.root, options, cancellation).await
    }
}

/// Compare an explicit Git worktree without requiring database connectivity.
/// # Errors
///
/// Returns an error when `root` is not a readable Git worktree, a ref cannot be
/// resolved, bounded Git output is unavailable or invalid, or comparison work
/// is cancelled.
pub async fn discover_source_comparison(
    root: impl AsRef<Path>,
    options: SourceCompareOptions,
) -> Result<SourceCompareReport, SourceCompareError> {
    compare_sources_at(root.as_ref(), options, ProjectCancellation::new()).await
}

pub(crate) async fn compare_sources_at(
    root: &Path,
    options: SourceCompareOptions,
    cancellation: ProjectCancellation,
) -> Result<SourceCompareReport, SourceCompareError> {
    let root = std::fs::canonicalize(root).map_err(|_| SourceCompareError::NotGitRepository)?;
    let repository = run_git(&root, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map_err(map_git_error)?;
    if !repository.success || crate::trim_ascii_bytes(&repository.stdout) != b"true" {
        return Err(SourceCompareError::NotGitRepository);
    }
    let base_commit = resolve_commit(&root, &options.base_ref).await?;
    let head_commit = match options.head_ref.as_deref() {
        Some(head) => Some(resolve_commit(&root, head).await?),
        None => None,
    };
    let mut paths = changed_paths(&root, &base_commit, head_commit.as_deref()).await?;
    let changed_before_filter = u64::try_from(paths.len()).unwrap_or(u64::MAX);
    if let Some(filter) = options.path_filter.as_deref() {
        paths.retain(|path| path.as_str().starts_with(filter));
    }
    let changed_after_filter = u64::try_from(paths.len()).unwrap_or(u64::MAX);
    let changed_files_truncated = paths.len() > usize::from(options.max_changed_files);
    paths.truncate(usize::from(options.max_changed_files));
    let task_root = root.clone();
    let task_base = base_commit.clone();
    let task_head = head_commit.clone();
    let task_options = options.clone();
    let mut files = stream::iter(paths)
        .map(|path| {
            let root = task_root.clone();
            let base = task_base.clone();
            let head = task_head.clone();
            let options = task_options.clone();
            let cancellation = cancellation.clone();
            async move {
                if cancellation.is_cancelled() {
                    return Err(SourceCompareError::Cancelled);
                }
                let baseline = git_blob(&root, &base, &path).await?;
                let current = match head.as_deref() {
                    Some(head) => git_blob(&root, head, &path).await?,
                    None => read_worktree_file(&root, &path).await?,
                };
                if cancellation.is_cancelled() {
                    return Err(SourceCompareError::Cancelled);
                }
                tokio::task::spawn_blocking(move || {
                    compare_one_file(
                        SourceFileComparison {
                            path,
                            baseline,
                            current,
                        },
                        &options,
                    )
                })
                .await
                .map_err(|_| SourceCompareError::InvalidOutput)?
            }
        })
        .buffer_unordered(COMPARE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let counts = source_comparison_counts(&files);
    let comparison = options.head_ref.as_ref().map_or_else(
        || options.base_ref.clone(),
        |head| {
            if head == &options.base_ref {
                head.clone()
            } else {
                format!("{}..{head}", options.base_ref)
            }
        },
    );
    Ok(SourceCompareReport {
        comparison,
        base_ref: options.base_ref,
        base_commit,
        head_ref: options.head_ref,
        head_commit,
        compares_worktree: task_head.is_none(),
        path_filter: options.path_filter,
        changed_before_filter,
        changed_after_filter,
        files_analyzed: counts.files_analyzed,
        files_skipped: counts.files_skipped,
        changed_files_truncated,
        symbols_added: counts.symbols_added,
        symbols_removed: counts.symbols_removed,
        symbols_modified: counts.symbols_modified,
        edges_added: counts.edges_added,
        edges_removed: counts.edges_removed,
        findings_introduced: counts.findings_introduced,
        findings_cleared: counts.findings_cleared,
        files,
    })
}

struct SourceComparisonCounts {
    files_analyzed: u64,
    files_skipped: u64,
    symbols_added: u64,
    symbols_removed: u64,
    symbols_modified: u64,
    edges_added: u64,
    edges_removed: u64,
    findings_introduced: u64,
    findings_cleared: u64,
}

fn source_comparison_counts(files: &[SourceFileDelta]) -> SourceComparisonCounts {
    let files_analyzed = files
        .iter()
        .filter(|file| file.skip_reason.is_none())
        .count();
    SourceComparisonCounts {
        files_analyzed: u64::try_from(files_analyzed).unwrap_or(u64::MAX),
        files_skipped: u64::try_from(files.len().saturating_sub(files_analyzed))
            .unwrap_or(u64::MAX),
        symbols_added: sum_lengths(files.iter().map(|file| file.added.len())),
        symbols_removed: sum_lengths(files.iter().map(|file| file.removed.len())),
        symbols_modified: sum_lengths(files.iter().map(|file| file.modified.len())),
        edges_added: sum_lengths(files.iter().map(|file| file.edges_added.len())),
        edges_removed: sum_lengths(files.iter().map(|file| file.edges_removed.len())),
        findings_introduced: sum_lengths(files.iter().filter_map(|file| {
            file.findings_delta
                .as_ref()
                .map(|delta| delta.introduced.len())
        })),
        findings_cleared: sum_lengths(files.iter().filter_map(|file| {
            file.findings_delta
                .as_ref()
                .map(|delta| delta.cleared.len())
        })),
    }
}

async fn resolve_commit(root: &Path, reference: &str) -> Result<String, SourceCompareError> {
    let expression = format!("{reference}^{{commit}}");
    let output = run_git(
        root,
        &["rev-parse", "--verify", "--end-of-options", &expression],
    )
    .await
    .map_err(map_git_error)?;
    if !output.success {
        return Err(SourceCompareError::RefNotFound);
    }
    let commit = std::str::from_utf8(crate::trim_ascii_bytes(&output.stdout))
        .ok()
        .filter(|value| {
            matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .ok_or(SourceCompareError::InvalidOutput)?;
    Ok(commit.to_ascii_lowercase())
}

async fn changed_paths(
    root: &Path,
    base_commit: &str,
    head_commit: Option<&str>,
) -> Result<Vec<NormalizedPath>, SourceCompareError> {
    let range;
    let comparison = if let Some(head) = head_commit {
        range = format!("{base_commit}..{head}");
        range.as_str()
    } else {
        base_commit
    };
    let output = run_git(
        root,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--relative",
            comparison,
            "--",
            ".",
        ],
    )
    .await
    .map_err(map_git_error)?;
    if !output.success {
        return Err(SourceCompareError::GitUnavailable);
    }
    let mut paths = parse_nul_paths(&output.stdout)?;
    if head_commit.is_none() {
        let untracked = run_git(
            root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
            ],
        )
        .await
        .map_err(map_git_error)?;
        if !untracked.success {
            return Err(SourceCompareError::GitUnavailable);
        }
        paths.extend(parse_nul_paths(&untracked.stdout)?);
    }
    Ok(paths
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn parse_nul_paths(output: &[u8]) -> Result<Vec<NormalizedPath>, SourceCompareError> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if output.last() != Some(&0) {
        return Err(SourceCompareError::InvalidOutput);
    }
    output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .map(|raw| {
            let path = std::str::from_utf8(raw).map_err(|_| SourceCompareError::InvalidOutput)?;
            NormalizedPath::parse(path).map_err(|_| SourceCompareError::InvalidOutput)
        })
        .collect()
}

async fn git_blob(
    root: &Path,
    commit: &str,
    path: &NormalizedPath,
) -> Result<Option<Vec<u8>>, SourceCompareError> {
    let object = format!("{commit}:{}", path.as_str());
    let output = run_git_bounded(
        root,
        &["cat-file", "blob", &object],
        GitCommandBounds::new(GIT_SOURCE_OUTPUT_BYTES, GIT_SOURCE_TIMEOUT),
    )
    .await
    .map_err(map_git_error)?;
    Ok(output.success.then_some(output.stdout))
}

async fn read_worktree_file(
    root: &Path,
    path: &NormalizedPath,
) -> Result<Option<Vec<u8>>, SourceCompareError> {
    let root = root.to_path_buf();
    let path = path.clone();
    tokio::task::spawn_blocking(move || read_bounded_worktree_file(&root, &path))
        .await
        .map_err(|_| SourceCompareError::InvalidOutput)?
}

fn read_bounded_worktree_file(
    root: &Path,
    path: &NormalizedPath,
) -> Result<Option<Vec<u8>>, SourceCompareError> {
    let candidate = root.join(path.as_str());
    let target = match std::fs::canonicalize(candidate) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(SourceCompareError::InvalidOutput),
    };
    if !target.starts_with(root) {
        return Err(SourceCompareError::InvalidOutput);
    }
    let metadata = std::fs::metadata(&target).map_err(|_| SourceCompareError::InvalidOutput)?;
    if !metadata.is_file()
        || metadata.len() > u64::try_from(GIT_SOURCE_OUTPUT_BYTES).unwrap_or(u64::MAX)
    {
        return Err(SourceCompareError::InvalidOutput);
    }
    std::fs::read(target)
        .map(Some)
        .map_err(|_| SourceCompareError::InvalidOutput)
}

struct SourceFileComparison {
    path: NormalizedPath,
    baseline: Option<Vec<u8>>,
    current: Option<Vec<u8>>,
}

fn compare_one_file(
    input: SourceFileComparison,
    options: &SourceCompareOptions,
) -> Result<SourceFileDelta, SourceCompareError> {
    let SourceFileComparison {
        path,
        baseline,
        current,
    } = input;
    let limits = source_limits().map_err(|_| SourceCompareError::InvalidOutput)?;
    let baseline = extract_side(&path, baseline, limits);
    let current = extract_side(&path, current, limits);
    let had_baseline = baseline.is_some();
    let had_current = current.is_some();
    let (baseline, current) = match (baseline, current) {
        (Some(Ok(baseline)), Some(Ok(current))) => (Some(baseline), Some(current)),
        (Some(Ok(baseline)), None) => (Some(baseline), None),
        (None, Some(Ok(current))) => (None, Some(current)),
        (None, None) => {
            return Ok(skipped_file(SkippedSourceFile {
                path,
                had_baseline: false,
                had_current: false,
                reason: "source_unavailable",
            }));
        }
        (Some(Err(reason)), _) | (_, Some(Err(reason))) => {
            return Ok(skipped_file(SkippedSourceFile {
                path,
                had_baseline,
                had_current,
                reason,
            }));
        }
    };
    let language = current
        .as_ref()
        .or(baseline.as_ref())
        .map(|file| file.language.as_str().to_owned());
    let symbol_diff = diff_symbols(SymbolDiffInput {
        baseline: baseline.as_ref(),
        current: current.as_ref(),
        include_findings: options.findings.include,
        suppress_line_range_only: options.delta.suppress_line_range_only,
    });
    let findings_delta = options
        .findings
        .delta
        .then(|| diff_findings(baseline.as_ref(), current.as_ref()));
    let (edges_added, edges_removed) = if options.delta.edges {
        diff_edges(baseline.as_ref(), current.as_ref())
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(SourceFileDelta {
        path,
        had_baseline,
        had_current,
        language,
        added: symbol_diff.added,
        removed: symbol_diff.removed,
        modified: symbol_diff.modified,
        line_range_only_count: symbol_diff.line_range_only_count,
        findings_delta,
        edges_added,
        edges_removed,
        skip_reason: None,
    })
}

fn extract_side(
    path: &NormalizedPath,
    source: Option<Vec<u8>>,
    limits: SourceLimits,
) -> Option<Result<ExtractedFile, &'static str>> {
    source.map(|source| {
        let snapshot = SourceSnapshot::from_bytes(path.as_str(), &source, limits)
            .map_err(|_| "unsupported_or_invalid_source")?;
        let mut extractor =
            NativeExtractor::new(snapshot.language()).map_err(|_| "extractor_unavailable")?;
        extractor
            .extract(&snapshot)
            .map_err(|_| "source_extraction_failed")
    })
}

struct SkippedSourceFile {
    path: NormalizedPath,
    had_baseline: bool,
    had_current: bool,
    reason: &'static str,
}

fn skipped_file(input: SkippedSourceFile) -> SourceFileDelta {
    let SkippedSourceFile {
        path,
        had_baseline,
        had_current,
        reason,
    } = input;
    SourceFileDelta {
        path,
        had_baseline,
        had_current,
        language: None,
        added: Vec::new(),
        removed: Vec::new(),
        modified: Vec::new(),
        line_range_only_count: 0,
        findings_delta: None,
        edges_added: Vec::new(),
        edges_removed: Vec::new(),
        skip_reason: Some(reason),
    }
}

#[derive(Default)]
struct SymbolDiff {
    added: Vec<SourceSymbolDelta>,
    removed: Vec<SourceSymbolDelta>,
    modified: Vec<SourceSymbolDelta>,
    line_range_only_count: u64,
}

#[derive(Clone, Copy)]
struct SymbolDiffInput<'a> {
    baseline: Option<&'a ExtractedFile>,
    current: Option<&'a ExtractedFile>,
    include_findings: bool,
    suppress_line_range_only: bool,
}

fn diff_symbols(input: SymbolDiffInput<'_>) -> SymbolDiff {
    let SymbolDiffInput {
        baseline,
        current,
        include_findings,
        suppress_line_range_only,
    } = input;
    let Some(path) = current.or(baseline).map(|file| &file.path) else {
        return SymbolDiff::default();
    };
    let before = grouped_symbols(baseline.map_or(&[][..], |file| file.symbols.as_slice()));
    let after = grouped_symbols(current.map_or(&[][..], |file| file.symbols.as_slice()));
    let keys = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut diff = SymbolDiff::default();
    for key in keys {
        let empty = Vec::new();
        let before_group = before.get(&key).unwrap_or(&empty);
        let after_group = after.get(&key).unwrap_or(&empty);
        for index in 0..before_group.len().max(after_group.len()) {
            record_symbol_pair(
                &mut diff,
                SymbolPairInput {
                    before: before_group.get(index).copied(),
                    after: after_group.get(index).copied(),
                    path,
                    include_findings,
                    suppress_line_range_only,
                },
            );
        }
    }
    diff
}

#[derive(Clone, Copy)]
struct SymbolPairInput<'symbol> {
    before: Option<&'symbol ExtractedSymbol>,
    after: Option<&'symbol ExtractedSymbol>,
    path: &'symbol NormalizedPath,
    include_findings: bool,
    suppress_line_range_only: bool,
}

#[derive(Clone, Copy)]
struct SymbolDeltaInput<'symbol, 'reason> {
    symbol: &'symbol ExtractedSymbol,
    path: &'symbol NormalizedPath,
    modified_reasons: &'reason [&'static str],
    include_findings: bool,
}

fn record_symbol_pair(diff: &mut SymbolDiff, input: SymbolPairInput<'_>) {
    match (input.before, input.after) {
        (None, Some(after)) => {
            diff.added.push(symbol_delta(SymbolDeltaInput {
                symbol: after,
                path: input.path,
                modified_reasons: &[],
                include_findings: input.include_findings,
            }));
        }
        (Some(before), None) => diff.removed.push(symbol_delta(SymbolDeltaInput {
            symbol: before,
            path: input.path,
            modified_reasons: &[],
            include_findings: false,
        })),
        (Some(before), Some(after)) => {
            let reasons = modification_reasons(before, after);
            if reasons.is_empty() {
                return;
            }
            if input.suppress_line_range_only && reasons == ["line_range_changed"] {
                diff.line_range_only_count = diff.line_range_only_count.saturating_add(1);
            } else {
                diff.modified.push(symbol_delta(SymbolDeltaInput {
                    symbol: after,
                    path: input.path,
                    modified_reasons: &reasons,
                    include_findings: input.include_findings,
                }));
            }
        }
        (None, None) => {}
    }
}

type SymbolKey = (String, String);

fn grouped_symbols(symbols: &[ExtractedSymbol]) -> BTreeMap<SymbolKey, Vec<&ExtractedSymbol>> {
    let mut groups = BTreeMap::<SymbolKey, Vec<&ExtractedSymbol>>::new();
    for symbol in symbols
        .iter()
        .filter(|symbol| !matches!(symbol.kind.as_str(), "file" | "import" | "parameter"))
    {
        groups
            .entry((
                symbol.qualified_name.clone(),
                symbol.kind.as_str().to_owned(),
            ))
            .or_default()
            .push(symbol);
    }
    for group in groups.values_mut() {
        group.sort_by_key(|symbol| (symbol.span.start_byte(), symbol.span.end_byte()));
    }
    groups
}

fn modification_reasons(before: &ExtractedSymbol, after: &ExtractedSymbol) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    if before.signature != after.signature {
        reasons.push("signature_changed");
    }
    if before.structural_digest != after.structural_digest {
        reasons.push("syntax_changed");
    }
    if before.span.start_line() != after.span.start_line()
        || before.span.end_line() != after.span.end_line()
    {
        reasons.push("line_range_changed");
    }
    if before.execution.async_symbol != after.execution.async_symbol {
        reasons.push("async_modifier_changed");
    }
    if before.execution.static_member != after.execution.static_member {
        reasons.push("static_modifier_changed");
    }
    if before.export != after.export {
        reasons.push("export_visibility_changed");
    }
    if before.visibility != after.visibility {
        reasons.push("declaration_visibility_changed");
    }
    if before.implementation.declaration_only != after.implementation.declaration_only {
        reasons.push("implementation_presence_changed");
    }
    reasons
}

fn symbol_delta(input: SymbolDeltaInput<'_, '_>) -> SourceSymbolDelta {
    SourceSymbolDelta {
        symbol_id: input.symbol.id.clone(),
        qualified_name: input.symbol.qualified_name.clone(),
        name: input.symbol.name.clone(),
        symbol_kind: input.symbol.kind.as_str().to_owned(),
        start_line: input.symbol.span.start_line(),
        end_line: input.symbol.span.end_line(),
        modified_reasons: input.modified_reasons.to_vec(),
        findings: if input.include_findings {
            findings_for_symbol(input.symbol, input.path)
        } else {
            Vec::new()
        },
    }
}

fn framework_convention_export(path: &NormalizedPath, symbol: &ExtractedSymbol) -> bool {
    let path = path.as_str();
    let name = qualified_leaf(&symbol.qualified_name);
    remix_convention_export(path, name, symbol.export.default_export)
        || next_convention_export(path, name)
        || root_convention_export(path, name)
}

fn remix_convention_export(path: &str, name: &str, default_export: bool) -> bool {
    if rooted_routes_path(path) && web_module_stem(path, false).is_some() && name == "Route" {
        return true;
    }
    let Some(relative) = app_relative_path(path) else {
        return false;
    };
    if relative.starts_with("routes/") && web_module_stem(relative, false).is_some() {
        return default_export || remix_route_export(name);
    }
    if !relative.contains('/') && web_module_stem(relative, false) == Some("root") {
        return default_export || remix_root_export(name);
    }
    !relative.contains('/')
        && web_module_stem(relative, false) == Some("entry.server")
        && (default_export || remix_entry_export(name))
}

fn next_convention_export(path: &str, name: &str) -> bool {
    let Some(relative) = app_relative_path(path) else {
        return false;
    };
    let Some(stem) = web_module_stem(relative, true) else {
        return false;
    };
    if next_config_file(stem) && next_config_export(name) {
        return true;
    }
    if stem == "route" && next_http_export(name) {
        return true;
    }
    next_metadata_file(stem) && next_metadata_export(name)
}

fn root_convention_export(path: &str, name: &str) -> bool {
    (matches!(path, "middleware.ts" | "middleware.js" | "middleware.mjs")
        || matches!(
            path,
            "src/middleware.ts" | "src/middleware.js" | "src/middleware.mjs"
        ))
        && matches!(name, "middleware" | "config")
        || (matches!(
            path,
            "instrumentation.ts" | "instrumentation.js" | "instrumentation.mjs"
        ) || matches!(
            path,
            "src/instrumentation.ts" | "src/instrumentation.js" | "src/instrumentation.mjs"
        )) && matches!(name, "register" | "onRequestError")
}

fn rooted_routes_path(path: &str) -> bool {
    path.starts_with("routes/") || path.contains("/routes/")
}

fn app_relative_path(path: &str) -> Option<&str> {
    path.strip_prefix("app/").or_else(|| {
        path.find("/app/")
            .map(|index| &path[index.saturating_add("/app/".len())..])
    })
}

fn web_module_stem(path: &str, allow_mjs: bool) -> Option<&str> {
    let filename = path.rsplit('/').next().unwrap_or(path);
    [".tsx", ".jsx", ".ts", ".js"]
        .into_iter()
        .find_map(|extension| filename.strip_suffix(extension))
        .or_else(|| allow_mjs.then(|| filename.strip_suffix(".mjs")).flatten())
}

fn qualified_leaf(qualified_name: &str) -> &str {
    qualified_name
        .rsplit([':', '.', '#', '/', '$'])
        .find(|component| !component.is_empty())
        .unwrap_or(qualified_name)
}

fn remix_route_export(name: &str) -> bool {
    matches!(
        name,
        "loader"
            | "clientLoader"
            | "action"
            | "clientAction"
            | "headers"
            | "handle"
            | "links"
            | "meta"
            | "shouldRevalidate"
            | "middleware"
            | "clientMiddleware"
            | "unstable_middleware"
            | "unstable_clientMiddleware"
            | "ErrorBoundary"
            | "HydrateFallback"
    )
}

fn remix_root_export(name: &str) -> bool {
    remix_route_export(name) || name == "Layout"
}

fn remix_entry_export(name: &str) -> bool {
    matches!(
        name,
        "handleRequest" | "handleDataRequest" | "handleError" | "streamTimeout"
    )
}

fn next_config_file(stem: &str) -> bool {
    matches!(
        stem,
        "page"
            | "layout"
            | "template"
            | "route"
            | "error"
            | "global-error"
            | "loading"
            | "not-found"
            | "default"
            | "sitemap"
            | "robots"
            | "manifest"
            | "icon"
            | "apple-icon"
            | "opengraph-image"
            | "twitter-image"
    )
}

fn next_config_export(name: &str) -> bool {
    matches!(
        name,
        "dynamic"
            | "dynamicParams"
            | "revalidate"
            | "fetchCache"
            | "runtime"
            | "preferredRegion"
            | "maxDuration"
            | "experimental_ppr"
            | "generateStaticParams"
            | "generateImageMetadata"
            | "generateSitemaps"
    )
}

fn next_http_export(name: &str) -> bool {
    matches!(
        name,
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    )
}

fn next_metadata_file(stem: &str) -> bool {
    next_config_file(stem) && stem != "route"
}

fn next_metadata_export(name: &str) -> bool {
    matches!(
        name,
        "metadata" | "generateMetadata" | "viewport" | "generateViewport"
    )
}

fn findings_for_symbol(symbol: &ExtractedSymbol, path: &NormalizedPath) -> Vec<SourceFinding> {
    let health = symbol.health;
    let parameter_metric =
        if symbol.implementation.declaration_only || framework_convention_export(path, symbol) {
            0.0
        } else {
            f64::from(health.parameter_count)
        };
    let mut findings = Vec::new();
    for (finding, metric, thresholds) in [
        (
            "large_method",
            f64::from(health.code_lines),
            LARGE_METHOD_THRESHOLDS,
        ),
        (
            "complex_method",
            f64::from(health.cyclomatic),
            COMPLEX_METHOD_THRESHOLDS,
        ),
        (
            "nested_complexity",
            f64::from(health.max_nesting),
            NESTED_COMPLEXITY_THRESHOLDS,
        ),
        (
            "complex_conditional",
            f64::from(health.max_conditional_operands),
            COMPLEX_CONDITIONAL_THRESHOLDS,
        ),
        (
            "long_parameter_list",
            parameter_metric,
            LONG_PARAMETER_THRESHOLDS,
        ),
    ]
    .into_iter()
    .chain(health_findings(health))
    {
        push_finding(
            &mut findings,
            SourceFindingInput {
                symbol,
                finding,
                metric,
                thresholds,
            },
        );
    }
    findings
}

fn health_findings(
    health: SymbolHealthMetrics,
) -> [(&'static str, f64, CompareFindingThreshold); 17] {
    [
        (
            "accidental_quadratic",
            f64::from(health.accidental_quadratic),
            ONE_TO_FIVE_THRESHOLDS,
        ),
        (
            "empty_catch",
            f64::from(health.empty_catches),
            ONE_TO_THREE_THRESHOLDS,
        ),
        (
            "sync_io_in_async",
            f64::from(health.sync_io_in_async),
            ONE_TO_THREE_THRESHOLDS,
        ),
        (
            "forof_await",
            f64::from(health.sequential_await_loops),
            TWO_TO_FIVE_THRESHOLDS,
        ),
        (
            "ts_any_cast",
            f64::from(health.ts_any_casts),
            THREE_TO_FIVE_THRESHOLDS,
        ),
        (
            "ts_ignore_suppression",
            f64::from(health.ts_suppressions),
            ONE_TO_THREE_THRESHOLDS,
        ),
        (
            "agent_debug_log",
            f64::from(health.debug_logs),
            ONE_TO_FIVE_THRESHOLDS,
        ),
        (
            "incomplete_marker",
            f64::from(health.incomplete_markers),
            FIVE_TO_FIFTY_THRESHOLDS,
        ),
        (
            "dynamic_eval",
            f64::from(health.dynamic_eval),
            ONE_TO_ONE_THRESHOLDS,
        ),
        (
            "insecure_hash",
            f64::from(health.insecure_hash),
            ONE_TO_THREE_THRESHOLDS,
        ),
        (
            "random_for_security",
            f64::from(health.insecure_random),
            ONE_TO_ONE_THRESHOLDS,
        ),
        (
            "http_no_timeout",
            f64::from(health.http_without_timeout),
            ONE_TO_THREE_THRESHOLDS,
        ),
        (
            "sql_string_concat",
            f64::from(health.sql_string_concatenation),
            ONE_TO_ONE_THRESHOLDS,
        ),
        (
            "unsafe_json_parse",
            f64::from(health.unsafe_json_parse),
            ONE_TO_THREE_THRESHOLDS,
        ),
        (
            "env_no_validation",
            f64::from(health.unvalidated_env),
            ONE_TO_THREE_THRESHOLDS,
        ),
        (
            "empty_function_body",
            f64::from(health.empty_body),
            EMPTY_BODY_THRESHOLDS,
        ),
        (
            "brain_method",
            f64::from(health.cyclomatic) / BRAIN_CYCLOMATIC_NORMALIZER
                + f64::from(health.max_nesting) / BRAIN_NESTING_NORMALIZER,
            BRAIN_METHOD_THRESHOLDS,
        ),
    ]
}

fn push_finding(findings: &mut Vec<SourceFinding>, input: SourceFindingInput<'_>) {
    if input.metric < input.thresholds.warning {
        return;
    }
    findings.push(SourceFinding {
        qualified_name: input.symbol.qualified_name.clone(),
        finding: input.finding,
        severity: if input.metric >= input.thresholds.error {
            "error"
        } else {
            "warning"
        },
        metric: input.metric,
    });
}

fn diff_findings(
    before: Option<&ExtractedFile>,
    after: Option<&ExtractedFile>,
) -> SourceFindingsDelta {
    let before = finding_map(before);
    let after = finding_map(after);
    let mut delta = SourceFindingsDelta::default();
    for (key, finding) in &after {
        if before.contains_key(key) {
            delta.carried.push(finding.clone());
        } else {
            delta.introduced.push(finding.clone());
        }
    }
    for (key, finding) in before {
        if !after.contains_key(&key) {
            delta.cleared.push(finding);
        }
    }
    delta
}

fn finding_map(file: Option<&ExtractedFile>) -> BTreeMap<(String, &'static str), SourceFinding> {
    file.into_iter()
        .flat_map(|file| {
            file.symbols
                .iter()
                .flat_map(|symbol| findings_for_symbol(symbol, &file.path))
        })
        .map(|finding| ((finding.qualified_name.clone(), finding.finding), finding))
        .collect()
}

fn diff_edges(
    baseline: Option<&ExtractedFile>,
    current: Option<&ExtractedFile>,
) -> (Vec<SourceEdgeDelta>, Vec<SourceEdgeDelta>) {
    let before = baseline.map_or_else(BTreeSet::new, edge_set);
    let after = current.map_or_else(BTreeSet::new, edge_set);
    (
        after.difference(&before).cloned().collect(),
        before.difference(&after).cloned().collect(),
    )
}

fn edge_set(file: &ExtractedFile) -> BTreeSet<SourceEdgeDelta> {
    let names = file
        .symbols
        .iter()
        .map(|symbol| (symbol.id.clone(), symbol.qualified_name.clone()))
        .collect::<BTreeMap<SymbolId, String>>();
    let mut edges = BTreeSet::new();
    for containment in &file.containments {
        if let (Some(source), Some(target)) = (
            names.get(&containment.parent),
            names.get(&containment.child),
        ) {
            edges.insert(SourceEdgeDelta {
                source: source.clone(),
                target: target.clone(),
                edge_kind: "contains".to_owned(),
            });
        }
    }
    for reference in &file.references {
        let source = reference
            .owner
            .as_ref()
            .and_then(|owner| names.get(owner))
            .cloned()
            .unwrap_or_else(|| file.path.as_str().to_owned());
        edges.insert(SourceEdgeDelta {
            source,
            target: reference.name.clone(),
            edge_kind: reference.kind.as_str().to_owned(),
        });
    }
    edges
}

fn sum_lengths(lengths: impl IntoIterator<Item = usize>) -> u64 {
    lengths.into_iter().fold(0_u64, |total, length| {
        total.saturating_add(u64::try_from(length).unwrap_or(u64::MAX))
    })
}

fn map_git_error(_error: crate::ReviewError) -> SourceCompareError {
    SourceCompareError::GitUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(path: &str, source: &str) -> ExtractedFile {
        let limits = SourceLimits::new(1024 * 1024)
            .unwrap_or_else(|error| panic!("source limits failed: {error}"));
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("snapshot failed: {error}"));
        NativeExtractor::new(snapshot.language())
            .and_then(|mut extractor| extractor.extract(&snapshot))
            .unwrap_or_else(|error| panic!("extraction failed: {error}"))
    }

    #[test]
    fn structural_digest_detects_body_only_edits_and_line_only_noise_can_collapse() {
        let before = extract("src/a.ts", "export function value() { return 1; }\n");
        let after = extract("src/a.ts", "export function value() { return 2; }\n");
        let diff = diff_symbols(SymbolDiffInput {
            baseline: Some(&before),
            current: Some(&after),
            include_findings: false,
            suppress_line_range_only: true,
        });
        assert_eq!(diff.modified.len(), 1);
        assert!(
            diff.modified[0]
                .modified_reasons
                .contains(&"syntax_changed")
        );

        let shifted = extract("src/a.ts", "\nexport function value() { return 1; }\n");
        let shifted = diff_symbols(SymbolDiffInput {
            baseline: Some(&before),
            current: Some(&shifted),
            include_findings: false,
            suppress_line_range_only: true,
        });
        assert_eq!(shifted.modified, []);
        assert_eq!(shifted.line_range_only_count, 1);
    }

    #[test]
    fn source_findings_use_executable_lines_for_static_jsx() {
        let mut source = String::from(
            "export function StaticPanel({ name }: { name: string }) {\n  return <section>\n",
        );
        for _ in 0..150 {
            source.push_str("    <span>Static presentation</span>\n");
        }
        source.push_str("    <span>{name.trim()}</span>\n  </section>;\n}\n");
        let file = extract("src/static-panel.tsx", &source);
        let panel = file
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "StaticPanel")
            .unwrap_or_else(|| panic!("missing StaticPanel symbol"));
        assert!(panel.span.end_line() > 100);
        assert!(
            !findings_for_symbol(panel, &file.path)
                .iter()
                .any(|finding| finding.finding == "large_method")
        );
    }

    #[test]
    fn source_findings_honor_framework_owned_parameter_conventions() {
        let route_before = extract(
            "app/api/precision/route.ts",
            "export async function GET(a: string, b: string, c: string, d: string) { return a; }\n",
        );
        let route = extract(
            "app/api/precision/route.ts",
            "export async function GET(a: string, b: string, c: string, d: string, e: string) { return a; }\n",
        );
        let get = route
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "GET")
            .unwrap_or_else(|| panic!("missing GET symbol"));
        assert_eq!(get.health.parameter_count, 5);
        assert!(
            !findings_for_symbol(get, &route.path)
                .iter()
                .any(|finding| finding.finding == "long_parameter_list")
        );
        let route_diff = diff_symbols(SymbolDiffInput {
            baseline: Some(&route_before),
            current: Some(&route),
            include_findings: true,
            suppress_line_range_only: true,
        });
        assert_eq!(route_diff.modified.len(), 1);
        assert!(
            !route_diff.modified[0]
                .findings
                .iter()
                .any(|finding| finding.finding == "long_parameter_list")
        );

        let ordinary = extract(
            "src/precision.ts",
            "export function processRequest(a: string, b: string, c: string, d: string, e: string) { return a; }\n",
        );
        let process_request = ordinary
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "processRequest")
            .unwrap_or_else(|| panic!("missing processRequest symbol"));
        assert!(
            findings_for_symbol(process_request, &ordinary.path)
                .iter()
                .any(|finding| finding.finding == "long_parameter_list")
        );
    }
}
