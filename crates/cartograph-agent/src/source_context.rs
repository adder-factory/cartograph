use cartograph_db::{
    CurrentFileLookup, CurrentFileRecord, CurrentSymbolRecord, CurrentSymbolSetLookup,
    ProjectSnapshot, StorageError,
};
use cartograph_domain::{
    ContentDigest, GenerationDigestVersion, GenerationId, NormalizedPath, SymbolId,
};
use serde::Serialize;

use crate::{ProjectCancellation, ProjectError, ProjectRuntime, utf8_boundary};

const DEFAULT_CONTEXT_LINES: u16 = 3;
const MAXIMUM_CONTEXT_LINES: u16 = 200;
const DEFAULT_EXCERPT_BYTES: usize = 64 * 1_024;
const MINIMUM_EXCERPT_BYTES: usize = 1_024;
const MAXIMUM_EXCERPT_BYTES: usize = 256 * 1_024;
const SOURCE_CONTEXT_ATTEMPTS: usize = 2;
const DEFAULT_FILE_LINE_LIMIT: u16 = 200;
const MAXIMUM_FILE_LINE_LIMIT: u16 = 500;
const MAXIMUM_FILE_EXCERPT_BYTES: usize = 256 * 1_024;
pub(crate) const MAXIMUM_CONTEXT_BATCH: usize = 500;
pub(crate) const MAXIMUM_CAPTURE_BATCH_BYTES: usize = 64 * 1_024 * 1_024;

/// Validated line and byte bounds for one source-context request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceContextOptions {
    context_lines: u16,
    maximum_bytes: usize,
    allow_stale_live_source: bool,
}

/// Exact symbol identity and bounded excerpt policy for one source read.
pub struct SourceContextRequest {
    symbol_id: SymbolId,
    options: SourceContextOptions,
}

/// Validated zero-based line window for one exact indexed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSourceOptions {
    line_offset: u32,
    line_limit: u16,
}

impl FileSourceOptions {
    /// Creates validated source-excerpt limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `line_limit` is zero or
    /// exceeds the maximum returned source lines.
    pub const fn new(line_offset: u32, line_limit: u16) -> Result<Self, ProjectError> {
        if line_limit == 0 || line_limit > MAXIMUM_FILE_LINE_LIMIT {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            line_offset,
            line_limit,
        })
    }
}

impl Default for FileSourceOptions {
    fn default() -> Self {
        Self {
            line_offset: 0,
            line_limit: DEFAULT_FILE_LINE_LIMIT,
        }
    }
}

/// Exact indexed path and bounded source-window policy.
pub struct FileSourceRequest {
    path: NormalizedPath,
    options: FileSourceOptions,
}

impl FileSourceRequest {
    #[must_use]
    /// Creates a validated file source request.
    pub const fn new(path: NormalizedPath, options: FileSourceOptions) -> Self {
        Self { path, options }
    }
}

/// Bounded UTF-8 source window with exact line and continuation evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSourceExcerpt {
    start_line: u32,
    end_line: u32,
    total_lines: u32,
    text: String,
    truncated: bool,
}

/// Indexed file identity plus live source only when its generation is fresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSourceContext {
    file: CurrentFileRecord,
    live_source_revision: ContentDigest,
    fresh: bool,
    file_fresh: bool,
    excerpt: Option<FileSourceExcerpt>,
}

impl FileSourceContext {
    #[must_use]
    /// Returns whether the live source still matches the indexed digest.
    pub const fn fresh(&self) -> bool {
        self.fresh
    }

    /// Whether this exact file still matches its indexed content hash.
    #[must_use]
    pub const fn file_fresh(&self) -> bool {
        self.file_fresh
    }
}

impl SourceContextRequest {
    /// Bind a current-generation symbol to validated output bounds.
    #[must_use]
    pub const fn new(symbol_id: SymbolId, options: SourceContextOptions) -> Self {
        Self { symbol_id, options }
    }
}

impl SourceContextOptions {
    /// Build symmetric surrounding-line and exact output-byte bounds.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `context_lines` is
    /// excessive or `maximum_bytes` is outside the accepted excerpt range.
    pub const fn new(context_lines: u16, maximum_bytes: usize) -> Result<Self, ProjectError> {
        if context_lines > MAXIMUM_CONTEXT_LINES
            || maximum_bytes < MINIMUM_EXCERPT_BYTES
            || maximum_bytes > MAXIMUM_EXCERPT_BYTES
        {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            context_lines,
            maximum_bytes,
            allow_stale_live_source: false,
        })
    }

    /// Explicitly allow a bounded live-disk slice when the indexed line range
    /// belongs to a file whose content no longer matches the generation.
    #[must_use]
    pub const fn with_stale_live_source(mut self, allow: bool) -> Self {
        self.allow_stale_live_source = allow;
        self
    }
}

impl Default for SourceContextOptions {
    fn default() -> Self {
        Self {
            context_lines: DEFAULT_CONTEXT_LINES,
            maximum_bytes: DEFAULT_EXCERPT_BYTES,
            allow_stale_live_source: false,
        }
    }
}

/// Bounded, line-numbered UTF-8 source text around one indexed declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SourceExcerpt {
    start_line: u32,
    end_line: u32,
    text: String,
    truncated: bool,
}

impl SourceExcerpt {
    /// First one-based source line included in `text`.
    #[must_use]
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    /// Last one-based source line represented in `text`.
    #[must_use]
    pub const fn end_line(&self) -> u32 {
        self.end_line
    }

    /// Exact bounded source text. A final newline is retained when present.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// True when the byte ceiling cut off the requested line range.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Agent-facing declaration metadata plus source only when the live checkout is fresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SymbolSourceContext {
    symbol: CurrentSymbolRecord,
    live_source_revision: ContentDigest,
    fresh: bool,
    excerpt: Option<SourceExcerpt>,
    live_source: bool,
}

impl SymbolSourceContext {
    /// Indexed declaration selected by exact identity.
    #[must_use]
    pub const fn symbol(&self) -> &CurrentSymbolRecord {
        &self.symbol
    }

    /// Whether the complete supported-source manifest matched the published generation.
    #[must_use]
    pub const fn fresh(&self) -> bool {
        self.fresh
    }

    /// Source excerpt, omitted rather than paired with stale indexed line numbers.
    #[must_use]
    pub const fn excerpt(&self) -> Option<&SourceExcerpt> {
        self.excerpt.as_ref()
    }

    /// Whether the excerpt is an explicitly requested approximate live-disk
    /// slice rather than an exact match for the indexed file content.
    #[must_use]
    pub const fn live_source(&self) -> bool {
        self.live_source
    }
}

impl ProjectRuntime {
    /// Read up to 500 symbol windows from one bounded source-manifest observation.
    ///
    /// All returned windows use the exact bytes hashed by that observation and
    /// belong to `expected_generation`. Missing symbols are omitted. Captured
    /// source is limited to 64 MiB across distinct files and is released on return.
    /// # Errors
    ///
    /// Returns an error for excessive requests, cancellation, source admission
    /// or capture failure, or a generation change. No partial batch is returned.
    pub async fn source_context_batch_with_cancellation(
        &self,
        expected_generation: &GenerationId,
        requests: Vec<SourceContextRequest>,
        cancellation: ProjectCancellation,
    ) -> Result<Vec<SymbolSourceContext>, ProjectError> {
        if requests.len() > MAXIMUM_CONTEXT_BATCH {
            return Err(ProjectError::InvalidOptions);
        }
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let before = self.source_context_snapshot().await?;
        let current = before
            .current
            .as_ref()
            .filter(|current| &current.generation_id == expected_generation)
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let ids = requests
            .iter()
            .map(|request| request.symbol_id.clone())
            .collect::<Vec<_>>();
        let symbols = self
            .database()
            .current_symbols_by_ids(CurrentSymbolSetLookup::new(
                &before.project_id,
                expected_generation,
                &ids,
            ))
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        let paths = symbols.iter().map(|symbol| symbol.path().clone()).collect();
        let source = self
            .scan_source_batch(
                paths,
                current.source_admission.run_excludes(),
                cancellation.clone(),
            )
            .await?;
        let fresh = current.source_revision == source.digest.as_str()
            && current.digest_version == GenerationDigestVersion::CURRENT;
        let mut contexts = Vec::new();
        contexts
            .try_reserve_exact(requests.len())
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        for request in requests {
            if cancellation.is_cancelled() {
                return Err(ProjectError::RequestCancelled);
            }
            let Some(symbol) = symbols
                .iter()
                .find(|symbol| symbol.symbol_id() == &request.symbol_id)
            else {
                continue;
            };
            let file = self
                .database()
                .exact_current_file_by_path(CurrentFileLookup::new(
                    &before.project_id,
                    expected_generation,
                    symbol.path(),
                ))
                .await
                .map_err(|_| ProjectError::SourceContextUnavailable)?
                .ok_or(ProjectError::SourceContextUnavailable)?;
            contexts.push(build_symbol_source_context(SymbolSourceContextInput {
                symbol: symbol.clone(),
                indexed_file: &file,
                source: &source,
                fresh,
                options: request.options,
            })?);
        }
        if !self.generation_is_current(&before).await? {
            return Err(ProjectError::SourceContextUnavailable);
        }
        Ok(contexts)
    }

    /// Resolve one exact current-generation symbol and return bounded source only
    /// when the complete live checkout still matches that generation.
    /// # Errors
    ///
    /// Returns an error when no stable current generation or exact symbol can
    /// be read, the bounded live file cannot be captured safely, or generation
    /// churn prevents a consistent result after the bounded retry.
    pub async fn source_context(
        &self,
        request: SourceContextRequest,
    ) -> Result<SymbolSourceContext, ProjectError> {
        self.source_context_with_cancellation(request, ProjectCancellation::new())
            .await
    }

    /// Resolve context while polling one caller-owned cancellation signal.
    /// # Errors
    ///
    /// Returns an error when no stable current generation or exact symbol can
    /// be read, the bounded live file cannot be captured safely, generation
    /// churn exhausts the retry, or `cancellation` wins.
    pub async fn source_context_with_cancellation(
        &self,
        request: SourceContextRequest,
        cancellation: ProjectCancellation,
    ) -> Result<SymbolSourceContext, ProjectError> {
        let SourceContextRequest { symbol_id, options } = request;
        for attempt in 0..SOURCE_CONTEXT_ATTEMPTS {
            let can_retry = attempt + 1 < SOURCE_CONTEXT_ATTEMPTS;
            if let Some(context) = self
                .source_context_attempt(SourceContextAttempt {
                    symbol_id: &symbol_id,
                    options,
                    cancellation: cancellation.clone(),
                    can_retry,
                })
                .await?
            {
                return Ok(context);
            }
        }
        Err(ProjectError::SourceContextUnavailable)
    }

    async fn source_context_attempt(
        &self,
        request: SourceContextAttempt<'_>,
    ) -> Result<Option<SymbolSourceContext>, ProjectError> {
        let SourceContextAttempt {
            symbol_id,
            options,
            cancellation,
            can_retry,
        } = request;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let before = self.source_context_snapshot().await?;
        let current = before
            .current
            .as_ref()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let Some(symbol) = self
            .source_context_symbol(&before, symbol_id, can_retry)
            .await?
        else {
            return Ok(None);
        };
        if symbol.generation_id() != &current.generation_id {
            return Ok(None);
        }
        let source = self
            .scan_source(Some(symbol.path().clone()), cancellation)
            .await?;
        if !self.generation_is_current(&before).await? {
            return if can_retry {
                Ok(None)
            } else {
                Err(ProjectError::SourceContextUnavailable)
            };
        }
        let indexed_file = self
            .database()
            .exact_current_file_by_path(CurrentFileLookup::new(
                &before.project_id,
                &current.generation_id,
                symbol.path(),
            ))
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let fresh = current.source_revision == source.digest.as_str()
            && current.digest_version == GenerationDigestVersion::CURRENT;
        Ok(Some(build_symbol_source_context(
            SymbolSourceContextInput {
                symbol,
                indexed_file: &indexed_file,
                source: &source,
                fresh,
                options,
            },
        )?))
    }

    async fn source_context_snapshot(&self) -> Result<ProjectSnapshot, ProjectError> {
        self.database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .ok_or(ProjectError::SourceContextUnavailable)
    }

    async fn source_context_symbol(
        &self,
        before: &ProjectSnapshot,
        symbol_id: &SymbolId,
        can_retry: bool,
    ) -> Result<Option<CurrentSymbolRecord>, ProjectError> {
        let current = before
            .current
            .as_ref()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let symbols = self
            .database()
            .current_symbols_by_ids(CurrentSymbolSetLookup::new(
                &before.project_id,
                &current.generation_id,
                std::slice::from_ref(symbol_id),
            ))
            .await;
        let mut symbols = match symbols {
            Ok(symbols) => symbols,
            Err(StorageError::CurrentGenerationChanged) if can_retry => return Ok(None),
            Err(_) => return Err(ProjectError::SourceContextUnavailable),
        };
        let Some(symbol) = symbols.pop() else {
            if self.generation_is_current(before).await? {
                return Err(ProjectError::SymbolNotFound);
            }
            return Ok(None);
        };
        Ok(Some(symbol))
    }

    /// Read a bounded line window from one exact indexed file when source is current.
    /// # Errors
    ///
    /// Returns an error when no stable current generation or exact file can be
    /// read, the bounded live file cannot be captured safely, generation churn
    /// exhausts the retry, or `cancellation` wins.
    pub async fn file_source_with_cancellation(
        &self,
        request: FileSourceRequest,
        cancellation: ProjectCancellation,
    ) -> Result<FileSourceContext, ProjectError> {
        let FileSourceRequest { path, options } = request;
        for attempt in 0..SOURCE_CONTEXT_ATTEMPTS {
            if cancellation.is_cancelled() {
                return Err(ProjectError::RequestCancelled);
            }
            let Some(before) = self
                .database()
                .project_snapshot_by_root(self.root_identity())
                .await
                .map_err(|_| ProjectError::SourceContextUnavailable)?
            else {
                return Err(ProjectError::SourceContextUnavailable);
            };
            let Some(current) = before.current.as_ref() else {
                return Err(ProjectError::SourceContextUnavailable);
            };
            let file = self
                .database()
                .exact_current_file_by_path(CurrentFileLookup::new(
                    &before.project_id,
                    &current.generation_id,
                    &path,
                ))
                .await;
            let file = match file {
                Ok(Some(file)) => file,
                Ok(None) if self.generation_is_current(&before).await? => {
                    return Err(ProjectError::FileNotFound);
                }
                Ok(None) | Err(StorageError::CurrentGenerationChanged)
                    if attempt + 1 < SOURCE_CONTEXT_ATTEMPTS =>
                {
                    continue;
                }
                Ok(None) | Err(_) => return Err(ProjectError::SourceContextUnavailable),
            };
            let source = self
                .scan_source(Some(path.clone()), cancellation.clone())
                .await?;
            if !self.generation_is_current(&before).await? {
                if attempt + 1 < SOURCE_CONTEXT_ATTEMPTS {
                    continue;
                }
                return Err(ProjectError::SourceContextUnavailable);
            }
            let fresh = current.source_revision == source.digest.as_str()
                && current.digest_version == GenerationDigestVersion::CURRENT;
            let capture = source.captures.get(&path);
            let file_fresh =
                capture.is_some_and(|capture| &capture.content_hash == file.content_hash());
            let excerpt = if file_fresh {
                let captured = capture
                    .map(|capture| capture.source.as_ref())
                    .ok_or(ProjectError::SourceContextUnavailable)?;
                Some(extract_file_excerpt(captured, options)?)
            } else {
                None
            };
            return Ok(FileSourceContext {
                file,
                live_source_revision: source.digest,
                fresh,
                file_fresh,
                excerpt,
            });
        }
        Err(ProjectError::SourceContextUnavailable)
    }

    async fn generation_is_current(
        &self,
        expected: &cartograph_db::ProjectSnapshot,
    ) -> Result<bool, ProjectError> {
        let observed = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        Ok(observed.as_ref().is_some_and(|snapshot| {
            snapshot.project_id == expected.project_id
                && snapshot
                    .current
                    .as_ref()
                    .map(|current| (&current.generation_id, current.source_revision.as_str()))
                    == expected
                        .current
                        .as_ref()
                        .map(|current| (&current.generation_id, current.source_revision.as_str()))
        }))
    }
}

struct SourceContextAttempt<'a> {
    symbol_id: &'a SymbolId,
    options: SourceContextOptions,
    cancellation: ProjectCancellation,
    can_retry: bool,
}

struct SymbolSourceContextInput<'a> {
    symbol: CurrentSymbolRecord,
    indexed_file: &'a CurrentFileRecord,
    source: &'a crate::SourceRevision,
    fresh: bool,
    options: SourceContextOptions,
}

fn build_symbol_source_context(
    input: SymbolSourceContextInput<'_>,
) -> Result<SymbolSourceContext, ProjectError> {
    let SymbolSourceContextInput {
        symbol,
        indexed_file,
        source,
        fresh,
        options,
    } = input;
    let capture = source.captures.get(symbol.path());
    let file_fresh =
        capture.is_some_and(|capture| &capture.content_hash == indexed_file.content_hash());
    let live_source = !file_fresh && options.allow_stale_live_source;
    let excerpt = if file_fresh || live_source {
        let captured = capture
            .map(|capture| capture.source.as_ref())
            .ok_or(ProjectError::SourceContextUnavailable)?;
        Some(extract_excerpt(ExcerptRequest {
            source: captured,
            symbol_start: symbol.start_line(),
            symbol_end: symbol.end_line(),
            options,
        })?)
    } else {
        None
    };
    Ok(SymbolSourceContext {
        symbol,
        live_source_revision: source.digest.clone(),
        fresh,
        excerpt,
        live_source,
    })
}

fn extract_file_excerpt(
    source: &str,
    options: FileSourceOptions,
) -> Result<FileSourceExcerpt, ProjectError> {
    let lines = source.split_inclusive('\n').collect::<Vec<_>>();
    let total_lines =
        u32::try_from(lines.len()).map_err(|_| ProjectError::SourceContextUnavailable)?;
    let offset =
        usize::try_from(options.line_offset).map_err(|_| ProjectError::SourceContextUnavailable)?;
    let mut text = String::new();
    text.try_reserve(MAXIMUM_FILE_EXCERPT_BYTES)
        .map_err(|_| ProjectError::SourceContextUnavailable)?;
    let mut returned = 0_u32;
    let mut byte_truncated = false;
    for line in lines
        .iter()
        .skip(offset)
        .take(usize::from(options.line_limit))
    {
        let remaining = MAXIMUM_FILE_EXCERPT_BYTES.saturating_sub(text.len());
        if line.len() > remaining {
            let boundary = utf8_boundary(line, remaining);
            text.push_str(&line[..boundary]);
            returned = returned.saturating_add(1);
            byte_truncated = true;
            break;
        }
        text.push_str(line);
        returned = returned.saturating_add(1);
    }
    let start_line = options.line_offset.saturating_add(1);
    let end_line = options.line_offset.saturating_add(returned);
    Ok(FileSourceExcerpt {
        start_line,
        end_line,
        total_lines,
        text,
        truncated: byte_truncated || end_line < total_lines,
    })
}

#[derive(Clone, Copy)]
struct ExcerptRequest<'source> {
    source: &'source str,
    symbol_start: u32,
    symbol_end: u32,
    options: SourceContextOptions,
}

fn extract_excerpt(input: ExcerptRequest<'_>) -> Result<SourceExcerpt, ProjectError> {
    let ExcerptRequest {
        source,
        symbol_start,
        symbol_end,
        options,
    } = input;
    let context = u32::from(options.context_lines);
    let requested_start = symbol_start.saturating_sub(context).max(1);
    let requested_end = symbol_end.saturating_add(context);
    let mut text = String::new();
    text.try_reserve(options.maximum_bytes)
        .map_err(|_| ProjectError::SourceContextUnavailable)?;
    let mut end_line = requested_start;
    let mut truncated = false;
    for (index, line) in source.split_inclusive('\n').enumerate() {
        let line_number = u32::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(ProjectError::SourceContextUnavailable)?;
        if line_number < requested_start {
            continue;
        }
        if line_number > requested_end {
            break;
        }
        let remaining = options.maximum_bytes.saturating_sub(text.len());
        if line.len() > remaining {
            let boundary = utf8_boundary(line, remaining);
            text.push_str(&line[..boundary]);
            end_line = line_number;
            truncated = true;
            break;
        }
        text.push_str(line);
        end_line = line_number;
    }
    if text.is_empty() {
        return Err(ProjectError::SourceContextUnavailable);
    }
    Ok(SourceExcerpt {
        start_line: requested_start,
        end_line,
        text,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_MAXIMUM_BYTES: usize = MINIMUM_EXCERPT_BYTES;
    const FIXTURE_SYMBOL_LINE: u32 = 3;
    const FIXTURE_CONTEXT_LINES: u16 = 1;

    #[test]
    fn excerpt_preserves_lines_and_reports_utf8_safe_truncation() {
        let source = "zero\none\n🦀two\nthree\nfour\n";
        let full = extract_excerpt(ExcerptRequest {
            source,
            symbol_start: FIXTURE_SYMBOL_LINE,
            symbol_end: FIXTURE_SYMBOL_LINE,
            options: SourceContextOptions::new(FIXTURE_CONTEXT_LINES, FIXTURE_MAXIMUM_BYTES)
                .unwrap_or_else(|error| panic!("source options failed: {error}")),
        })
        .unwrap_or_else(|error| panic!("source excerpt failed: {error}"));
        assert_eq!(full.start_line(), 2);
        assert_eq!(full.end_line(), 4);
        assert_eq!(full.text(), "one\n🦀two\nthree\n");
        assert!(!full.truncated());

        let long = format!("{}🦀", "x".repeat(FIXTURE_MAXIMUM_BYTES));
        let truncated = extract_excerpt(ExcerptRequest {
            source: &long,
            symbol_start: 1,
            symbol_end: 1,
            options: SourceContextOptions::new(0, FIXTURE_MAXIMUM_BYTES)
                .unwrap_or_else(|error| panic!("source options failed: {error}")),
        })
        .unwrap_or_else(|error| panic!("truncated excerpt failed: {error}"));
        assert_eq!(truncated.text().len(), FIXTURE_MAXIMUM_BYTES);
        assert_eq!(truncated.text(), "x".repeat(FIXTURE_MAXIMUM_BYTES));
        assert!(truncated.truncated());
    }

    #[test]
    fn source_context_options_reject_unbounded_output() {
        assert_eq!(
            SourceContextOptions::new(
                MAXIMUM_CONTEXT_LINES.saturating_add(1),
                DEFAULT_EXCERPT_BYTES
            ),
            Err(ProjectError::InvalidOptions)
        );
        assert_eq!(
            SourceContextOptions::new(
                DEFAULT_CONTEXT_LINES,
                MAXIMUM_EXCERPT_BYTES.saturating_add(1)
            ),
            Err(ProjectError::InvalidOptions)
        );
    }

    #[test]
    fn captured_excerpt_bytes_are_the_exact_bytes_used_by_the_manifest() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let file = directory.path().join("service.rs");
        std::fs::write(&file, "pub fn before() {}\n")
            .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
        let path = cartograph_domain::NormalizedPath::parse("service.rs")
            .unwrap_or_else(|error| panic!("fixture path failed: {error}"));
        let revision = crate::source_revision_with_capture(directory.path(), Some(&path), || false)
            .unwrap_or_else(|error| panic!("source revision failed: {error}"));

        std::fs::write(&file, "pub fn after() {}\n")
            .unwrap_or_else(|error| panic!("fixture mutation failed: {error}"));
        let captured = revision.captures.get(&path).map_or_else(
            || panic!("target source was not captured"),
            |capture| capture.source.as_ref(),
        );
        let excerpt = extract_excerpt(ExcerptRequest {
            source: captured,
            symbol_start: 1,
            symbol_end: 1,
            options: SourceContextOptions::new(0, FIXTURE_MAXIMUM_BYTES)
                .unwrap_or_else(|error| panic!("source options failed: {error}")),
        })
        .unwrap_or_else(|error| panic!("source excerpt failed: {error}"));
        assert_eq!(excerpt.text(), "pub fn before() {}\n");
        assert!(!excerpt.text().contains("after"));
    }

    #[test]
    fn file_excerpt_uses_zero_based_offsets_and_reports_continuation() {
        let excerpt = extract_file_excerpt(
            "zero\none\ntwo\nthree\n",
            FileSourceOptions::new(1, 2)
                .unwrap_or_else(|error| panic!("file options failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("file excerpt failed: {error}"));
        assert_eq!(excerpt.start_line, 2);
        assert_eq!(excerpt.end_line, 3);
        assert_eq!(excerpt.total_lines, 4);
        assert_eq!(excerpt.text, "one\ntwo\n");
        assert!(excerpt.truncated);
    }
}
