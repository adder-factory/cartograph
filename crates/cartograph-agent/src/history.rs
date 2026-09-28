use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use cartograph_config::load_project_source_settings;
use cartograph_db::{
    FileCochangeFact, FileCochangeMetrics, FileHistoryFact, FileHistoryMetrics,
    HistoryRefreshInput, HistoryRefreshMetadata, HistoryRefreshParameters, HistoryRefreshReport,
    HistoryRefreshRequest,
};
use cartograph_domain::{NormalizedPath, ProjectId};
use num_traits::ToPrimitive;
use thiserror::Error;

use crate::{
    ProjectCancellation, ProjectRuntime, ReviewError,
    review::{GitCommandBounds, run_git, run_git_bounded},
};

const DEFAULT_MAX_COMMITS: u32 = 20_000;
const MAX_HISTORY_COMMITS: u32 = 50_000;
const MAX_GIT_HISTORY_BYTES: usize = 128 * 1_048_576;
const GIT_HISTORY_TIMEOUT: Duration = Duration::from_mins(2);
const MAX_FILES_PER_COCHANGE_COMMIT: usize = 512;
const MAX_COCHANGE_PAIRS: usize = 2_000_000;
const COMMIT_MARKER: &[u8] = b"CARTOGRAPH_COMMIT";
const CANCELLATION_POLL_FIELDS: usize = 4_096;
const COMMIT_FIELD_COUNT: usize = 4;
const COMMIT_AUTHOR_FIELD_OFFSET: usize = 3;
const RENAMED_NUMSTAT_FIELD_COUNT: usize = 3;
/// Churn/co-change mining semantics. Bump whenever parsing, bounds or
/// aggregation change so stored refreshes are never reused across them.
const HISTORY_ALGORITHM_VERSION: u32 = 1;

/// Bounded full-repository Git mining options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryIndexOptions {
    max_commits: u32,
}

impl Default for HistoryIndexOptions {
    fn default() -> Self {
        Self {
            max_commits: DEFAULT_MAX_COMMITS,
        }
    }
}

impl HistoryIndexOptions {
    /// Sets the max commits and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryIndexError::InvalidOptions`] when `value` is zero or
    /// exceeds the bounded commit-history ceiling.
    pub const fn with_max_commits(mut self, value: u32) -> Result<Self, HistoryIndexError> {
        if value == 0 || value > MAX_HISTORY_COMMITS {
            return Err(HistoryIndexError::InvalidOptions);
        }
        self.max_commits = value;
        Ok(self)
    }
}

/// Safe full-history indexing failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum HistoryIndexError {
    #[error("history indexing options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("history indexing requires a Git repository with HEAD")]
    /// The project root is not inside the required Git repository.
    NotGitRepository,
    #[error("Git history is unavailable or exceeded its runtime bound")]
    /// The bounded Git subprocess could not be executed.
    GitUnavailable,
    #[error("Git history output is malformed or exceeded its byte bound")]
    /// Git returned output that violates the bounded parser contract.
    GitOutputInvalid,
    #[error("history indexing exceeded its deterministic relation bound")]
    /// Derived relationships exceeded their declared ceiling.
    RelationLimit,
    #[error("history indexing was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
    #[error("history persistence failed")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
    #[error("Git churn and co-change analysis are disabled by project configuration")]
    /// Project configuration explicitly disables this derivation.
    DisabledByProjectConfig,
}

impl ProjectRuntime {
    /// Mine bounded HEAD history and atomically persist churn/co-change evidence.
    /// # Errors
    ///
    /// Returns an error when project configuration is invalid or disables both
    /// history channels, the checkout lacks readable bounded Git history,
    /// derived relation limits are exceeded, cancellation wins, or persistence fails.
    pub async fn refresh_git_history(
        &self,
        project_id: ProjectId,
        options: HistoryIndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<HistoryRefreshReport, HistoryIndexError> {
        let settings = load_project_source_settings(&self.root)
            .map_err(|_| HistoryIndexError::InvalidOptions)?;
        let churn = settings.enable_churn();
        let co_change = settings.enable_co_change();
        if !churn && !co_change {
            self.database()
                .clear_file_history(&project_id)
                .await
                .map_err(|_| HistoryIndexError::StorageUnavailable)?;
            return Err(HistoryIndexError::DisabledByProjectConfig);
        }
        let mut prepared = self
            .prepare_git_history(options, cancellation)
            .await?
            .with_channels(churn, co_change);
        // An explicit refresh always rescans, and records its inputs so the
        // next index run at the same HEAD can reuse it.
        prepared.parameters = Some(self.history_parameters(options, (churn, co_change)).await?);
        self.persist_git_history(project_id, prepared).await
    }

    pub(crate) async fn prepare_git_history(
        &self,
        options: HistoryIndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<PreparedHistoryIndex, HistoryIndexError> {
        if cancellation.is_cancelled() {
            return Err(HistoryIndexError::Cancelled);
        }
        let head = git_head(&self.root).await?;
        let shallow = git_is_shallow(&self.root).await?;
        let requested = options
            .max_commits
            .checked_add(1)
            .ok_or(HistoryIndexError::InvalidOptions)?
            .to_string();
        let output = run_git_bounded(
            &self.root,
            &[
                "log",
                "-z",
                "--find-renames",
                "--format=CARTOGRAPH_COMMIT%x00%H%x00%ct%x00%ae%x00",
                "--numstat",
                "-n",
                &requested,
                "HEAD",
                "--",
            ],
            GitCommandBounds::new(MAX_GIT_HISTORY_BYTES, GIT_HISTORY_TIMEOUT),
        )
        .await
        .map_err(map_git_error)?;
        if !output.success {
            return Err(HistoryIndexError::GitUnavailable);
        }
        let scan = parse_git_log(&output.stdout, options.max_commits, &cancellation)?;
        if cancellation.is_cancelled() {
            return Err(HistoryIndexError::Cancelled);
        }
        let files = scan
            .files
            .into_iter()
            .map(|(path, facts)| {
                FileHistoryFact::new(
                    path,
                    FileHistoryMetrics {
                        commit_count: facts.commit_count,
                        author_count: u64::try_from(facts.authors.len()).unwrap_or(u64::MAX),
                        insertions: facts.insertions,
                        deletions: facts.deletions,
                        last_touched_at: Some(facts.last_touched_at),
                    },
                )
                .map_err(|_| HistoryIndexError::RelationLimit)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let cochanges = scan
            .cochanges
            .into_iter()
            .map(|((path_a, path_b), shared)| {
                let count_a = scan
                    .file_commit_counts
                    .get(&path_a)
                    .copied()
                    .ok_or(HistoryIndexError::GitOutputInvalid)?;
                let count_b = scan
                    .file_commit_counts
                    .get(&path_b)
                    .copied()
                    .ok_or(HistoryIndexError::GitOutputInvalid)?;
                let union = count_a
                    .checked_add(count_b)
                    .and_then(|value| value.checked_sub(shared))
                    .filter(|value| *value > 0)
                    .ok_or(HistoryIndexError::RelationLimit)?;
                let confidence = (shared.to_f64().ok_or(HistoryIndexError::RelationLimit)?
                    / union.to_f64().ok_or(HistoryIndexError::RelationLimit)?)
                .to_f32()
                .ok_or(HistoryIndexError::RelationLimit)?;
                FileCochangeFact::new(
                    path_a,
                    path_b,
                    FileCochangeMetrics {
                        commit_count: shared,
                        confidence,
                    },
                )
                .map_err(|_| HistoryIndexError::RelationLimit)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PreparedHistoryIndex {
            head,
            metadata: HistoryRefreshMetadata {
                shallow_history: shallow,
                commits_scanned: scan.commits_scanned,
                truncated: scan.truncated,
                oversized_commits_skipped: scan.oversized_commits_skipped,
            },
            files,
            cochanges,
            parameters: None,
            reused: None,
        })
    }

    /// Prepare history for an index run, reusing the stored refresh when HEAD,
    /// shallowness and every refresh input are unchanged. Git history depends
    /// only on those inputs, so a reused refresh skips both the bounded
    /// `git log` scan and the churn/co-change rewrite.
    pub(crate) async fn prepare_git_history_reusing(
        &self,
        options: HistoryIndexOptions,
        channels: (bool, bool),
        cancellation: ProjectCancellation,
    ) -> Result<PreparedHistoryIndex, HistoryIndexError> {
        let (churn, co_change) = channels;
        let head = git_head(&self.root).await?;
        let shallow = git_is_shallow(&self.root).await?;
        let parameters = self.history_parameters(options, channels).await?;
        if let Ok(Some(report)) = self
            .database()
            .reusable_history_refresh(&self.root_identity, &head, shallow, parameters)
            .await
        {
            return Ok(PreparedHistoryIndex::reused(
                head,
                HistoryRefreshMetadata {
                    shallow_history: shallow,
                    commits_scanned: 0,
                    truncated: false,
                    oversized_commits_skipped: 0,
                },
                report,
            ));
        }
        let mut prepared = self
            .prepare_git_history(options, cancellation)
            .await?
            .with_channels(churn, co_change);
        prepared.parameters = Some(parameters);
        Ok(prepared)
    }

    async fn history_parameters(
        &self,
        options: HistoryIndexOptions,
        (churn, co_change): (bool, bool),
    ) -> Result<HistoryRefreshParameters, HistoryIndexError> {
        let max_commits = u64::from(options.max_commits);
        Ok(HistoryRefreshParameters {
            max_commits,
            commits_available: git_commit_count(&self.root, max_commits.saturating_add(1)).await?,
            algorithm_version: HISTORY_ALGORITHM_VERSION,
            churn,
            co_change,
        })
    }

    pub(crate) async fn persist_git_history(
        &self,
        project_id: ProjectId,
        prepared: PreparedHistoryIndex,
    ) -> Result<HistoryRefreshReport, HistoryIndexError> {
        if let Some(report) = prepared.reused {
            return Ok(report);
        }
        let parameters = prepared.parameters;
        let request = HistoryRefreshRequest::new(
            project_id,
            prepared.head,
            HistoryRefreshInput {
                metadata: prepared.metadata,
                files: prepared.files,
                cochanges: prepared.cochanges,
            },
        )
        .map_err(|_| HistoryIndexError::RelationLimit)?;
        let request = match parameters {
            Some(parameters) => request.with_parameters(parameters),
            None => request,
        };
        self.database()
            .replace_file_history(request)
            .await
            .map_err(|_| HistoryIndexError::StorageUnavailable)
    }
}

pub(crate) struct PreparedHistoryIndex {
    head: String,
    metadata: HistoryRefreshMetadata,
    files: Vec<FileHistoryFact>,
    cochanges: Vec<FileCochangeFact>,
    parameters: Option<HistoryRefreshParameters>,
    reused: Option<HistoryRefreshReport>,
}

impl PreparedHistoryIndex {
    fn reused(
        head: String,
        metadata: HistoryRefreshMetadata,
        report: HistoryRefreshReport,
    ) -> Self {
        Self {
            head,
            metadata,
            files: Vec::new(),
            cochanges: Vec::new(),
            parameters: None,
            reused: Some(report),
        }
    }

    pub(crate) fn with_channels(mut self, churn: bool, co_change: bool) -> Self {
        if !co_change {
            self.cochanges.clear();
        }
        if !churn && !co_change {
            self.files.clear();
        }
        self
    }
}

#[derive(Default)]
struct FileAggregate {
    commit_count: u64,
    authors: BTreeSet<[u8; 16]>,
    insertions: u64,
    deletions: u64,
    last_touched_at: u64,
}

struct HistoryScan {
    files: BTreeMap<NormalizedPath, FileAggregate>,
    file_commit_counts: BTreeMap<NormalizedPath, u64>,
    cochanges: BTreeMap<(NormalizedPath, NormalizedPath), u64>,
    commits_scanned: u64,
    truncated: bool,
    oversized_commits_skipped: u64,
}

#[derive(Clone, Copy)]
struct CommitMetadata {
    unix_seconds: u64,
    author_key: [u8; 16],
}

#[derive(Default)]
struct FileDelta {
    insertions: u64,
    deletions: u64,
}

#[derive(Default)]
struct CommitHistory {
    files: BTreeMap<NormalizedPath, FileAggregate>,
    cochanges: BTreeMap<(NormalizedPath, NormalizedPath), u64>,
    commits_scanned: u64,
    oversized_commits_skipped: u64,
}

impl CommitHistory {
    fn finish(self, truncated: bool) -> HistoryScan {
        let file_commit_counts = self
            .files
            .iter()
            .map(|(path, facts)| (path.clone(), facts.commit_count))
            .collect();
        HistoryScan {
            files: self.files,
            file_commit_counts,
            cochanges: self.cochanges,
            commits_scanned: self.commits_scanned,
            truncated,
            oversized_commits_skipped: self.oversized_commits_skipped,
        }
    }
}

fn parse_git_log(
    output: &[u8],
    max_commits: u32,
    cancellation: &ProjectCancellation,
) -> Result<HistoryScan, HistoryIndexError> {
    if max_commits == 0 || max_commits > MAX_HISTORY_COMMITS {
        return Err(HistoryIndexError::InvalidOptions);
    }
    let fields = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut index = 0_usize;
    let mut current = None;
    let mut changes = BTreeMap::new();
    let mut history = CommitHistory::default();
    let mut truncated = false;
    while index < fields.len() {
        if index.is_multiple_of(CANCELLATION_POLL_FIELDS) && cancellation.is_cancelled() {
            return Err(HistoryIndexError::Cancelled);
        }
        let raw_field = fields[index];
        if crate::trim_ascii_bytes(raw_field) == COMMIT_MARKER {
            apply_pending_commit(&mut history, &mut current, &mut changes)?;
            if history.commits_scanned >= u64::from(max_commits) {
                truncated = true;
                break;
            }
            current = Some(parse_commit_metadata(&fields, index)?);
            index += COMMIT_FIELD_COUNT;
            continue;
        }
        let field = trim_numstat_prefix(raw_field);
        if !field.is_empty() {
            index += parse_history_delta(HistoryDeltaInput {
                fields: &fields,
                index,
                field,
                has_commit: current.is_some(),
                changes: &mut changes,
            })?;
            continue;
        }
        index += 1;
    }
    if !truncated {
        apply_pending_commit(&mut history, &mut current, &mut changes)?;
    }
    Ok(history.finish(truncated))
}

fn apply_pending_commit(
    history: &mut CommitHistory,
    current: &mut Option<CommitMetadata>,
    changes: &mut BTreeMap<NormalizedPath, FileDelta>,
) -> Result<(), HistoryIndexError> {
    let Some(metadata) = current.take() else {
        return Ok(());
    };
    history.apply_commit(metadata, changes)?;
    changes.clear();
    Ok(())
}

fn parse_commit_metadata(
    fields: &[&[u8]],
    index: usize,
) -> Result<CommitMetadata, HistoryIndexError> {
    let commit = text_field(fields.get(index + 1).copied())?;
    if !valid_commit(commit) {
        return Err(HistoryIndexError::GitOutputInvalid);
    }
    let unix_seconds = text_field(fields.get(index + 2).copied())?
        .parse::<u64>()
        .map_err(|_| HistoryIndexError::GitOutputInvalid)?;
    let author = fields
        .get(index + COMMIT_AUTHOR_FIELD_OFFSET)
        .copied()
        .ok_or(HistoryIndexError::GitOutputInvalid)?;
    Ok(CommitMetadata {
        unix_seconds,
        author_key: author_key(author),
    })
}

struct HistoryDeltaInput<'fields, 'changes> {
    fields: &'fields [&'fields [u8]],
    index: usize,
    field: &'fields [u8],
    has_commit: bool,
    changes: &'changes mut BTreeMap<NormalizedPath, FileDelta>,
}

fn parse_history_delta(input: HistoryDeltaInput<'_, '_>) -> Result<usize, HistoryIndexError> {
    let HistoryDeltaInput {
        fields,
        index,
        field,
        has_commit,
        changes,
    } = input;
    if !has_commit {
        return Err(HistoryIndexError::GitOutputInvalid);
    }
    let (delta, renamed) = parse_numstat(field)?;
    if !renamed {
        add_delta(changes, delta.path.as_bytes(), delta)?;
        return Ok(1);
    }
    let new_path = fields
        .get(index + 2)
        .copied()
        .ok_or(HistoryIndexError::GitOutputInvalid)?;
    add_delta(changes, new_path, delta)?;
    Ok(RENAMED_NUMSTAT_FIELD_COUNT)
}

#[derive(Clone, Copy)]
struct ParsedNumstat<'a> {
    insertions: u64,
    deletions: u64,
    path: &'a str,
}

fn parse_numstat(field: &[u8]) -> Result<(ParsedNumstat<'_>, bool), HistoryIndexError> {
    let field = std::str::from_utf8(field).map_err(|_| HistoryIndexError::GitOutputInvalid)?;
    let mut columns = field.splitn(3, '\t');
    let insertions = parse_numstat_count(columns.next())?;
    let deletions = parse_numstat_count(columns.next())?;
    let path = columns.next().ok_or(HistoryIndexError::GitOutputInvalid)?;
    Ok((
        ParsedNumstat {
            insertions,
            deletions,
            path,
        },
        path.is_empty(),
    ))
}

fn parse_numstat_count(value: Option<&str>) -> Result<u64, HistoryIndexError> {
    match value.ok_or(HistoryIndexError::GitOutputInvalid)? {
        "-" => Ok(0),
        value => value
            .parse::<u64>()
            .map_err(|_| HistoryIndexError::GitOutputInvalid),
    }
}

fn add_delta(
    changes: &mut BTreeMap<NormalizedPath, FileDelta>,
    raw_path: &[u8],
    delta: ParsedNumstat<'_>,
) -> Result<(), HistoryIndexError> {
    let path = std::str::from_utf8(raw_path)
        .ok()
        .and_then(|path| NormalizedPath::parse(path).ok())
        .ok_or(HistoryIndexError::GitOutputInvalid)?;
    let existing = changes.entry(path).or_default();
    existing.insertions = existing
        .insertions
        .checked_add(delta.insertions)
        .ok_or(HistoryIndexError::RelationLimit)?;
    existing.deletions = existing
        .deletions
        .checked_add(delta.deletions)
        .ok_or(HistoryIndexError::RelationLimit)?;
    Ok(())
}

impl CommitHistory {
    fn apply_commit(
        &mut self,
        metadata: CommitMetadata,
        changes: &BTreeMap<NormalizedPath, FileDelta>,
    ) -> Result<(), HistoryIndexError> {
        for (path, delta) in changes {
            let aggregate = self.files.entry(path.clone()).or_default();
            aggregate.commit_count = aggregate.commit_count.saturating_add(1);
            aggregate.authors.insert(metadata.author_key);
            aggregate.insertions = aggregate
                .insertions
                .checked_add(delta.insertions)
                .ok_or(HistoryIndexError::RelationLimit)?;
            aggregate.deletions = aggregate
                .deletions
                .checked_add(delta.deletions)
                .ok_or(HistoryIndexError::RelationLimit)?;
            aggregate.last_touched_at = aggregate.last_touched_at.max(metadata.unix_seconds);
        }
        self.commits_scanned = self.commits_scanned.saturating_add(1);
        if changes.len() > MAX_FILES_PER_COCHANGE_COMMIT {
            self.oversized_commits_skipped = self.oversized_commits_skipped.saturating_add(1);
            return Ok(());
        }
        let paths = changes.keys().collect::<Vec<_>>();
        for (left_index, path_a) in paths.iter().enumerate() {
            for path_b in &paths[left_index.saturating_add(1)..] {
                if self.cochanges.len() >= MAX_COCHANGE_PAIRS
                    && !self
                        .cochanges
                        .contains_key(&((*path_a).clone(), (*path_b).clone()))
                {
                    return Err(HistoryIndexError::RelationLimit);
                }
                let count = self
                    .cochanges
                    .entry(((*path_a).clone(), (*path_b).clone()))
                    .or_default();
                *count = count.saturating_add(1);
            }
        }
        Ok(())
    }
}

async fn git_head(root: &Path) -> Result<String, HistoryIndexError> {
    let output = run_git(root, &["rev-parse", "--verify", "HEAD"])
        .await
        .map_err(map_git_error)?;
    if !output.success {
        return Err(HistoryIndexError::NotGitRepository);
    }
    let head = text_field(Some(crate::trim_ascii_bytes(&output.stdout)))?.to_ascii_lowercase();
    valid_commit(&head)
        .then_some(head)
        .ok_or(HistoryIndexError::GitOutputInvalid)
}

/// Commits reachable from HEAD, counted up to `limit`. Part of the history
/// reuse key: deepening a shallow clone keeps HEAD but changes this count.
async fn git_commit_count(root: &Path, limit: u64) -> Result<u64, HistoryIndexError> {
    let limit = limit.to_string();
    let output = run_git(
        root,
        &["rev-list", "--count", "--max-count", &limit, "HEAD"],
    )
    .await
    .map_err(map_git_error)?;
    if !output.success {
        return Err(HistoryIndexError::NotGitRepository);
    }
    std::str::from_utf8(crate::trim_ascii_bytes(&output.stdout))
        .ok()
        .and_then(|count| count.parse::<u64>().ok())
        .ok_or(HistoryIndexError::GitOutputInvalid)
}

async fn git_is_shallow(root: &Path) -> Result<bool, HistoryIndexError> {
    let output = run_git(root, &["rev-parse", "--is-shallow-repository"])
        .await
        .map_err(map_git_error)?;
    if !output.success {
        return Err(HistoryIndexError::NotGitRepository);
    }
    match crate::trim_ascii_bytes(&output.stdout) {
        b"true" => Ok(true),
        b"false" => Ok(false),
        _ => Err(HistoryIndexError::GitOutputInvalid),
    }
}

fn map_git_error(error: ReviewError) -> HistoryIndexError {
    match error {
        ReviewError::InvalidOptions | ReviewError::InvalidRef => HistoryIndexError::InvalidOptions,
        ReviewError::NotGitRepository | ReviewError::GitRefNotFound => {
            HistoryIndexError::NotGitRepository
        }
        ReviewError::GitOutputLimitExceeded | ReviewError::GitOutputInvalid => {
            HistoryIndexError::GitOutputInvalid
        }
        ReviewError::GitUnavailable
        | ReviewError::GitDeadlineExceeded
        | ReviewError::GitCommandFailed
        | ReviewError::ProjectRootUnavailable
        | ReviewError::ProjectStateUnavailable
        | ReviewError::RetrievalUnavailable => HistoryIndexError::GitUnavailable,
    }
}

fn author_key(author: &[u8]) -> [u8; 16] {
    let digest = blake3::hash(author);
    let mut key = [0_u8; 16];
    key.copy_from_slice(&digest.as_bytes()[..16]);
    key
}

fn text_field(value: Option<&[u8]>) -> Result<&str, HistoryIndexError> {
    std::str::from_utf8(value.ok_or(HistoryIndexError::GitOutputInvalid)?)
        .map_err(|_| HistoryIndexError::GitOutputInvalid)
}

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn trim_numstat_prefix(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b'\n' | b'\r')) {
        value = &value[1..];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_log_parser_aggregates_churn_authors_and_symmetric_pairs() {
        let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let output = format!(
            "CARTOGRAPH_COMMIT\0{first}\01700000002\0a@example.invalid\0\0\n3\t1\tsrc/a.rs\02\t0\tsrc/b.rs\0CARTOGRAPH_COMMIT\0{second}\01700000001\0b@example.invalid\0\0\n1\t2\tsrc/a.rs\0"
        );
        let scan = parse_git_log(output.as_bytes(), 10, &ProjectCancellation::new())
            .unwrap_or_else(|error| panic!("history parse failed: {error}"));
        assert_eq!(scan.commits_scanned, 2);
        let a = NormalizedPath::parse("src/a.rs")
            .unwrap_or_else(|error| panic!("path failed: {error}"));
        let b = NormalizedPath::parse("src/b.rs")
            .unwrap_or_else(|error| panic!("path failed: {error}"));
        assert_eq!(scan.files[&a].commit_count, 2);
        assert_eq!(scan.files[&a].authors.len(), 2);
        assert_eq!(scan.files[&a].insertions, 4);
        assert_eq!(scan.files[&a].deletions, 3);
        assert_eq!(scan.cochanges[&(a, b)], 1);
        assert!(!scan.truncated);
    }
}
