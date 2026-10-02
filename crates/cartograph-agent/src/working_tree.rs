use std::collections::BTreeSet;

use cartograph_domain::NormalizedPath;
use cartograph_extract::{SourceLimits, SourceReadOptions, SourceRoot};
use cartograph_search::{
    CONTEXT_QUERY_MAXIMUM_BYTES, WORKING_TREE_OVERLAY_MAXIMUM_EXCERPT_BYTES,
    WORKING_TREE_OVERLAY_MAXIMUM_FILES, WORKING_TREE_OVERLAY_MAXIMUM_RESULTS,
    WORKING_TREE_OVERLAY_MAXIMUM_SOURCE_BYTES, WorkingTreeChangeKind, WorkingTreeEvidence,
    WorkingTreeEvidenceInput, WorkingTreeOverlay, WorkingTreeOverlayInput,
};

use crate::{
    GitChangeKind, ProjectCancellation, ProjectError, ProjectRuntime, ReviewOptions,
    discover_git_comparison, utf8_boundary,
};

const OVERLAY_MAXIMUM_FILE_BYTES: usize = 512 * 1_024;
const OVERLAY_CONTEXT_LINES: usize = 2;
const OVERLAY_MAXIMUM_TERMS: usize = 16;
const OVERLAY_MINIMUM_TERM_BYTES: usize = 2;
const OVERLAY_MAXIMUM_TERM_BYTES: usize = 128;
const OVERLAY_STOP_WORDS: &[&str] = &[
    "add",
    "an",
    "and",
    "change",
    "code",
    "explain",
    "find",
    "fix",
    "for",
    "from",
    "help",
    "how",
    "in",
    "implement",
    "improve",
    "into",
    "make",
    "modify",
    "of",
    "on",
    "remove",
    "rename",
    "replace",
    "show",
    "that",
    "the",
    "this",
    "to",
    "trace",
    "use",
    "what",
    "where",
    "which",
    "with",
];

/// Bounded natural-language task and cancellation signal for a live overlay scan.
pub struct WorkingTreeOverlayRequest {
    task: String,
    cancellation: ProjectCancellation,
}

impl WorkingTreeOverlayRequest {
    /// Bind the same task used for durable retrieval to caller-owned cancellation.
    #[must_use]
    pub fn new(task: impl Into<String>, cancellation: ProjectCancellation) -> Self {
        Self {
            task: task.into(),
            cancellation,
        }
    }
}

impl ProjectRuntime {
    /// Search changed and untracked supported source without mixing it into the durable generation.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] for an empty, oversized, or
    /// NUL-containing task; [`ProjectError::RequestCancelled`] when cancellation
    /// wins; or a retrieval error when bounded overlay evidence cannot be formed.
    /// Git or individual source-read failures otherwise return explicit
    /// unavailable/unreadable evidence rather than failing the request.
    pub async fn working_tree_overlay(
        &self,
        request: WorkingTreeOverlayRequest,
    ) -> Result<WorkingTreeOverlay, ProjectError> {
        if request.task.trim().is_empty()
            || request.task.len() > CONTEXT_QUERY_MAXIMUM_BYTES
            || request.task.contains('\0')
        {
            return Err(ProjectError::InvalidOptions);
        }
        let options = ReviewOptions::new("HEAD")
            .and_then(|value| value.with_max_changed_files(WORKING_TREE_OVERLAY_MAXIMUM_FILES))
            .map_err(|_| ProjectError::InvalidOptions)?;
        let comparison = tokio::select! {
            biased;
            () = request.cancellation.cancelled() => return Err(ProjectError::RequestCancelled),
            result = discover_git_comparison(&self.root, &options) => match result {
                Ok(comparison) => comparison,
                Err(_) => return Ok(WorkingTreeOverlay::unavailable()),
            },
        };
        if comparison.files().is_empty() {
            return Ok(WorkingTreeOverlay::clean());
        }
        let candidates = comparison
            .files()
            .iter()
            .filter_map(overlay_candidate)
            .collect::<Vec<_>>();
        let root = self.root.clone();
        let terms = task_terms(&request.task);
        let changed_file_count = comparison.files().len();
        let comparison_truncated = comparison.truncated();
        if terms.is_empty() {
            return WorkingTreeOverlay::completed(WorkingTreeOverlayInput {
                changed_file_count,
                considered_file_count: 0,
                unreadable_file_count: 0,
                files: Vec::new(),
                truncated: comparison_truncated,
            })
            .map_err(|_| ProjectError::RetrievalOperationFailed);
        }
        let cancellation = request.cancellation.clone();
        let scan = match tokio::task::spawn_blocking(move || {
            scan_live_sources(OverlayScanRequest {
                root,
                candidates,
                terms,
                cancellation,
                truncated: comparison_truncated,
            })
        })
        .await
        {
            Ok(Ok(scan)) => scan,
            Ok(Err(ProjectError::RequestCancelled)) => return Err(ProjectError::RequestCancelled),
            Ok(Err(_)) | Err(_) => return Ok(WorkingTreeOverlay::unavailable()),
        };
        WorkingTreeOverlay::completed(WorkingTreeOverlayInput {
            changed_file_count,
            considered_file_count: scan.considered_file_count,
            unreadable_file_count: scan.unreadable_file_count,
            files: scan.files,
            truncated: scan.truncated,
        })
        .map_err(|_| ProjectError::RetrievalOperationFailed)
    }
}

#[derive(Clone)]
struct OverlayCandidate {
    path: NormalizedPath,
    change_kind: WorkingTreeChangeKind,
}

struct OverlayScan {
    considered_file_count: usize,
    unreadable_file_count: usize,
    files: Vec<WorkingTreeEvidence>,
    truncated: bool,
}

struct OverlayScanRequest {
    root: std::path::PathBuf,
    candidates: Vec<OverlayCandidate>,
    terms: Vec<String>,
    cancellation: ProjectCancellation,
    truncated: bool,
}

fn overlay_candidate(file: &crate::GitChangedFile) -> Option<OverlayCandidate> {
    let change_kind = match file.kind() {
        GitChangeKind::Added => WorkingTreeChangeKind::Added,
        GitChangeKind::Modified => WorkingTreeChangeKind::Modified,
        GitChangeKind::TypeChanged => WorkingTreeChangeKind::TypeChanged,
        GitChangeKind::Untracked => WorkingTreeChangeKind::Untracked,
        GitChangeKind::Deleted | GitChangeKind::Unmerged => return None,
    };
    Some(OverlayCandidate {
        path: file.path().clone(),
        change_kind,
    })
}

fn task_terms(task: &str) -> Vec<String> {
    let mut terms = BTreeSet::new();
    for term in task
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() >= OVERLAY_MINIMUM_TERM_BYTES)
        .filter(|term| !OVERLAY_STOP_WORDS.contains(&term.as_str()))
    {
        if term.len() <= OVERLAY_MAXIMUM_TERM_BYTES {
            terms.insert(term);
        }
        if terms.len() == OVERLAY_MAXIMUM_TERMS {
            break;
        }
    }
    terms.into_iter().collect()
}

fn scan_live_sources(input: OverlayScanRequest) -> Result<OverlayScan, ProjectError> {
    let OverlayScanRequest {
        root,
        candidates,
        terms,
        cancellation,
        mut truncated,
    } = input;
    let source_root =
        SourceRoot::open(&root).map_err(|_| ProjectError::SourceContextUnavailable)?;
    let mut considered_file_count = 0;
    let mut unreadable_file_count = 0;
    let mut retained_bytes = 0_usize;
    let mut matches = Vec::new();
    for candidate in candidates {
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let remaining = WORKING_TREE_OVERLAY_MAXIMUM_SOURCE_BYTES.saturating_sub(retained_bytes);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let maximum = remaining.min(OVERLAY_MAXIMUM_FILE_BYTES);
        let limits = SourceLimits::new(maximum).map_err(|_| ProjectError::InvalidOptions)?;
        considered_file_count += 1;
        let Ok(snapshot) = source_root.read_with_cancellation(
            &candidate.path,
            SourceReadOptions::new(limits, || cancellation.is_cancelled()),
        ) else {
            unreadable_file_count += 1;
            continue;
        };
        retained_bytes = retained_bytes.saturating_add(snapshot.source().len());
        if let Some(evidence) = match_source(&snapshot, candidate.change_kind, &terms)? {
            matches.push(evidence);
        }
    }
    matches.sort_by(|left, right| {
        right
            .match_count()
            .cmp(&left.match_count())
            .then_with(|| left.path().cmp(right.path()))
    });
    if matches.len() > WORKING_TREE_OVERLAY_MAXIMUM_RESULTS {
        matches.truncate(WORKING_TREE_OVERLAY_MAXIMUM_RESULTS);
        truncated = true;
    }
    Ok(OverlayScan {
        considered_file_count,
        unreadable_file_count,
        files: matches,
        truncated,
    })
}

fn match_source(
    snapshot: &cartograph_extract::SourceSnapshot,
    change_kind: WorkingTreeChangeKind,
    terms: &[String],
) -> Result<Option<WorkingTreeEvidence>, ProjectError> {
    if terms.is_empty() {
        return Ok(None);
    }
    let source = snapshot.source();
    let lowercase = source.to_ascii_lowercase();
    let matched_terms = terms
        .iter()
        .filter(|term| lowercase.contains(term.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if matched_terms.is_empty() {
        return Ok(None);
    }
    let first_line = source
        .lines()
        .position(|line| {
            let line = line.to_ascii_lowercase();
            matched_terms.iter().any(|term| line.contains(term))
        })
        .unwrap_or_default();
    let excerpt = live_excerpt(source, first_line);
    WorkingTreeEvidence::new(WorkingTreeEvidenceInput {
        path: snapshot.path().clone(),
        change_kind,
        content_digest: snapshot.content_hash().clone(),
        start_line: excerpt.start_line,
        end_line: excerpt.end_line,
        excerpt: excerpt.text,
        matched_terms,
    })
    .map(Some)
    .map_err(|_| ProjectError::RetrievalOperationFailed)
}

struct LiveExcerpt {
    start_line: u32,
    end_line: u32,
    text: String,
}

fn live_excerpt(source: &str, matched_line: usize) -> LiveExcerpt {
    let start = matched_line.saturating_sub(OVERLAY_CONTEXT_LINES);
    let end = matched_line.saturating_add(OVERLAY_CONTEXT_LINES);
    let mut text = String::new();
    let mut end_line = start;
    for (index, line) in source.split_inclusive('\n').enumerate() {
        if index < start {
            continue;
        }
        if index > end {
            break;
        }
        let remaining = WORKING_TREE_OVERLAY_MAXIMUM_EXCERPT_BYTES.saturating_sub(text.len());
        if remaining == 0 {
            break;
        }
        let boundary = utf8_boundary(line, remaining.min(line.len()));
        text.push_str(&line[..boundary]);
        end_line = index;
        if boundary < line.len() {
            break;
        }
    }
    LiveExcerpt {
        start_line: u32::try_from(start.saturating_add(1)).unwrap_or(u32::MAX),
        end_line: u32::try_from(end_line.saturating_add(1)).unwrap_or(u32::MAX),
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_terms_are_bounded_deduplicated_and_drop_generic_work_words() {
        assert_eq!(
            task_terms("Fix auth auth token parsing in the code"),
            vec!["auth", "parsing", "token"]
        );
        assert_eq!(task_terms("fix the code"), [] as [String; 0]);
    }

    #[test]
    fn live_overlay_reads_supported_changed_source_and_orders_stronger_matches_first() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::write(
            directory.path().join("auth.rs"),
            "pub fn parse_token() {\n    validate_auth_token();\n}\n",
        )
        .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
        std::fs::write(directory.path().join("other.rs"), "pub fn token() {}\n")
            .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
        let candidate = |path: &str| OverlayCandidate {
            path: NormalizedPath::parse(path)
                .unwrap_or_else(|error| panic!("fixture path failed: {error}")),
            change_kind: WorkingTreeChangeKind::Modified,
        };
        let scan = scan_live_sources(OverlayScanRequest {
            root: directory.path().to_path_buf(),
            candidates: vec![candidate("other.rs"), candidate("auth.rs")],
            terms: vec!["auth".to_owned(), "token".to_owned()],
            cancellation: ProjectCancellation::new(),
            truncated: false,
        })
        .unwrap_or_else(|error| panic!("overlay scan failed: {error}"));
        assert_eq!(scan.files.len(), 2);
        assert_eq!(scan.files[0].path().as_str(), "auth.rs");
        assert_eq!(scan.files[0].match_count(), 2);
        assert!(!scan.truncated);
    }
}
