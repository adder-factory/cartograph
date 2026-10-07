use std::{path::Path, sync::Arc};

use cartograph_db::{
    CartographDatabase, CurrentGenerationLookup, CurrentSourceRangeLookup, FileSurfaceQuery,
    FileSurfaceRow, SourceLineRange, StorageError,
};
use cartograph_domain::{GenerationId, NormalizedPath, ProjectId, SourceLanguage};
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use regex::{Regex, RegexBuilder};
use regex_syntax::{Error as RegexSyntaxError, ParserBuilder as RegexSyntaxParserBuilder};
use serde::Serialize;
use thiserror::Error;

use crate::{ProjectCancellation, ProjectRuntime};

const MAXIMUM_PATTERN_BYTES: usize = 1_024;
const MAXIMUM_RESULTS: u16 = 500;
const MAXIMUM_INDEXED_FILES: u16 = 2_000;
const MAXIMUM_FILE_BYTES: u64 = 1_000_000;
const MAXIMUM_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAXIMUM_RETAINED_HITS_PER_FILE: usize = 500;
const MAXIMUM_SNIPPET_CHARACTERS: usize = 200;
const SCAN_CONCURRENCY: usize = 8;
const ATTRIBUTION_CONCURRENCY: usize = 16;
const ENCLOSING_SYMBOL_LIMIT: u16 = 20;
const REGEX_SIZE_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// Validated bounded live-source regex search over only current indexed files.
#[derive(Clone)]
pub struct SourceSearchOptions {
    pattern: String,
    case_sensitive: bool,
    path_filter: Option<NormalizedPath>,
    language: Option<SourceLanguage>,
    limit: u16,
}

impl SourceSearchOptions {
    /// Creates validated source-search limits.
    ///
    /// # Errors
    ///
    /// Returns [`SourceSearchError::InvalidOptions`] when the pattern is empty,
    /// oversized, or contains NUL, or when `limit` is zero or excessive.
    pub fn new(pattern: impl Into<String>, limit: u16) -> Result<Self, SourceSearchError> {
        let pattern = pattern.into();
        if pattern.is_empty()
            || pattern.len() > MAXIMUM_PATTERN_BYTES
            || pattern.contains('\0')
            || limit == 0
            || limit > MAXIMUM_RESULTS
        {
            return Err(SourceSearchError::InvalidOptions);
        }
        Ok(Self {
            pattern,
            case_sensitive: false,
            path_filter: None,
            language: None,
            limit,
        })
    }

    #[must_use]
    /// Sets the case sensitive and returns the updated value.
    pub const fn with_case_sensitive(mut self, case_sensitive: bool) -> Self {
        self.case_sensitive = case_sensitive;
        self
    }

    #[must_use]
    /// Restrict results to paths containing one literal normalized substring.
    pub fn with_path_filter(mut self, path_filter: Option<NormalizedPath>) -> Self {
        self.path_filter = path_filter;
        self
    }

    #[must_use]
    /// Sets the language and returns the updated value.
    pub const fn with_language(mut self, language: Option<SourceLanguage>) -> Self {
        self.language = language;
        self
    }
}

/// Smallest indexed declaration enclosing one regex hit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSearchSymbol {
    symbol_id: String,
    qualified_name: String,
    symbol_kind: String,
    start_line: u32,
    end_line: u32,
}

/// One bounded line match from an indexed project file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSearchHit {
    path: NormalizedPath,
    language: String,
    line: u32,
    snippet: String,
    enclosing_symbol: Option<SourceSearchSymbol>,
}

/// Complete accounting for a parallel live-source search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSearchReport {
    hits: Vec<SourceSearchHit>,
    total_matches_in_scanned_files: u64,
    indexed_files: u64,
    files_scanned: u64,
    files_skipped_large: u64,
    files_unreadable_or_unsafe: u64,
    bytes_considered: u64,
    file_inventory_truncated: bool,
    byte_budget_truncated: bool,
    result_truncated: bool,
    regex_engine: &'static str,
    attribution: &'static str,
}

impl SourceSearchHit {
    #[must_use]
    /// Returns the path.
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    #[must_use]
    /// Returns the language.
    pub fn language(&self) -> &str {
        &self.language
    }

    #[must_use]
    /// Returns the line.
    pub const fn line(&self) -> u32 {
        self.line
    }

    #[must_use]
    /// Returns the snippet.
    pub fn snippet(&self) -> &str {
        &self.snippet
    }
}

impl SourceSearchReport {
    #[must_use]
    /// Returns the hits.
    pub fn hits(&self) -> &[SourceSearchHit] {
        &self.hits
    }
}

/// Safe public failure classes for live source search.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SourceSearchError {
    #[error("source search options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("source search regex is invalid at byte {offset}: {reason}")]
    /// The supplied regular expression has one source-safe parser diagnostic.
    InvalidRegex {
        /// Fixed parser category that never includes the supplied pattern.
        reason: String,
        /// Zero-based byte offset of the invalid syntax.
        offset: usize,
    },
    #[error("source search regex exceeds the compiled size bound")]
    /// The expression parsed but exceeded the bounded compiled representation.
    RegexTooLarge,
    #[error("source search storage is unavailable")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
    #[error("source search project root is unavailable")]
    /// The requested project could not be opened safely.
    ProjectUnavailable,
    #[error("source search was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
    #[error("source search worker failed")]
    /// A supervised worker failed before producing a complete result.
    WorkerFailed,
}

struct FileScanResult {
    hits: Vec<SourceSearchHit>,
    matched: u64,
    scanned: bool,
    skipped_large: bool,
    unreadable_or_unsafe: bool,
}

impl ProjectRuntime {
    /// Search current indexed source files with Rust's linear-time regex engine,
    /// then attribute retained hits to the smallest indexed declaration.
    /// # Errors
    ///
    /// Returns an error when the regex cannot be compiled within its size
    /// limits, current file/range storage or the project root is unavailable,
    /// a supervised scan worker fails, or cancellation wins.
    pub async fn search_source(
        &self,
        project_id: &ProjectId,
        options: SourceSearchOptions,
        cancellation: ProjectCancellation,
    ) -> Result<SourceSearchReport, SourceSearchError> {
        if cancellation.is_cancelled() {
            return Err(SourceSearchError::Cancelled);
        }
        let regex = compile_source_regex(&options)?;
        let generation = self
            .database
            .current_generation_record(project_id)
            .await
            .map_err(map_storage)?
            .ok_or(SourceSearchError::StorageUnavailable)?;
        let surface_query = source_surface_query(&options)?;
        let surface = self
            .database
            .current_file_surface(project_id, &surface_query)
            .await
            .map_err(map_storage)?;
        let root = std::fs::canonicalize(&self.root)
            .ok()
            .filter(|path| path.is_dir())
            .ok_or(SourceSearchError::ProjectUnavailable)?;
        let mut selected = Vec::new();
        let mut bytes_considered = 0_u64;
        let mut byte_budget_truncated = false;
        for file in surface.files() {
            if bytes_considered.saturating_add(file.byte_size()) > MAXIMUM_TOTAL_BYTES {
                byte_budget_truncated = true;
                break;
            }
            bytes_considered = bytes_considered.saturating_add(file.byte_size());
            selected.push(file.clone());
        }
        let root = Arc::new(root);
        let regex = Arc::new(regex);
        let cancellation = Arc::new(cancellation);
        let scan_results = stream::iter(selected)
            .map(|file| {
                let root = root.clone();
                let regex = regex.clone();
                let cancellation = cancellation.clone();
                async move {
                    tokio::task::spawn_blocking(move || {
                        scan_file(FileScanRequest {
                            root: &root,
                            file: &file,
                            regex: &regex,
                            cancellation: &cancellation,
                        })
                    })
                    .await
                    .map_err(|_| SourceSearchError::WorkerFailed)
                }
            })
            .buffered(SCAN_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
        if cancellation.is_cancelled() {
            return Err(SourceSearchError::Cancelled);
        }
        let summary = summarize_source_scans(&scan_results, options.limit);
        let attributed = attribute_source_hits(SourceAttributionRequest {
            database: self.database.clone(),
            project_id: project_id.clone(),
            generation_id: generation.generation_id().clone(),
            hits: summary.hits,
        })
        .await?;
        Ok(SourceSearchReport {
            hits: attributed,
            total_matches_in_scanned_files: summary.total_matches,
            indexed_files: surface.total_files(),
            files_scanned: summary.files_scanned,
            files_skipped_large: summary.files_skipped_large,
            files_unreadable_or_unsafe: summary.files_unreadable_or_unsafe,
            bytes_considered,
            file_inventory_truncated: surface.truncated(),
            byte_budget_truncated,
            result_truncated: summary.result_truncated,
            regex_engine: "rust_regex_linear_time",
            attribution: "smallest_current_generation_enclosing_symbol",
        })
    }
}

/// Translate validated search filters into the bounded indexed-file inventory query.
fn source_surface_query(
    options: &SourceSearchOptions,
) -> Result<FileSurfaceQuery, SourceSearchError> {
    let mut query = FileSurfaceQuery::new(MAXIMUM_INDEXED_FILES).map_err(map_storage)?;
    if let Some(language) = options.language {
        query = query.with_language(language);
    }
    if let Some(path_filter) = options.path_filter.as_ref() {
        let postgres_regex = regex::escape(path_filter.as_str());
        query = query
            .with_path_regex(Some(&postgres_regex))
            .map_err(map_storage)?;
    }
    Ok(query)
}

fn compile_source_regex(options: &SourceSearchOptions) -> Result<Regex, SourceSearchError> {
    let mut syntax = RegexSyntaxParserBuilder::new();
    syntax.case_insensitive(!options.case_sensitive);
    syntax
        .build()
        .parse(&options.pattern)
        .map_err(source_regex_syntax_error)?;
    RegexBuilder::new(&options.pattern)
        .case_insensitive(!options.case_sensitive)
        .multi_line(false)
        .size_limit(REGEX_SIZE_LIMIT_BYTES)
        .dfa_size_limit(REGEX_SIZE_LIMIT_BYTES)
        .build()
        .map_err(|error| match error {
            regex::Error::CompiledTooBig(_) => SourceSearchError::RegexTooLarge,
            _ => SourceSearchError::InvalidRegex {
                reason: "unsupported regex syntax".to_owned(),
                offset: 0,
            },
        })
}

fn source_regex_syntax_error(error: RegexSyntaxError) -> SourceSearchError {
    match error {
        RegexSyntaxError::Parse(error) => SourceSearchError::InvalidRegex {
            reason: error.kind().to_string(),
            offset: error.span().start.offset,
        },
        RegexSyntaxError::Translate(error) => SourceSearchError::InvalidRegex {
            reason: error.kind().to_string(),
            offset: error.span().start.offset,
        },
        _ => SourceSearchError::InvalidRegex {
            reason: "unsupported regex syntax".to_owned(),
            offset: 0,
        },
    }
}

struct SourceScanSummary {
    hits: Vec<SourceSearchHit>,
    total_matches: u64,
    files_scanned: u64,
    files_skipped_large: u64,
    files_unreadable_or_unsafe: u64,
    result_truncated: bool,
}

fn summarize_source_scans(results: &[FileScanResult], limit: u16) -> SourceScanSummary {
    let files_scanned = results.iter().filter(|result| result.scanned).count() as u64;
    let files_skipped_large = results.iter().filter(|result| result.skipped_large).count() as u64;
    let files_unreadable_or_unsafe = results
        .iter()
        .filter(|result| result.unreadable_or_unsafe)
        .count() as u64;
    let total_matches = results.iter().map(|result| result.matched).sum::<u64>();
    let maximum_depth = results
        .iter()
        .map(|result| result.hits.len())
        .max()
        .unwrap_or_default();
    let mut hits = Vec::new();
    'depth: for depth in 0..maximum_depth {
        for result in results {
            if let Some(hit) = result.hits.get(depth) {
                if hits.len() == usize::from(limit) {
                    break 'depth;
                }
                hits.push(hit.clone());
            }
        }
    }
    SourceScanSummary {
        result_truncated: total_matches > u64::try_from(hits.len()).unwrap_or(u64::MAX),
        hits,
        total_matches,
        files_scanned,
        files_skipped_large,
        files_unreadable_or_unsafe,
    }
}

struct SourceAttributionRequest {
    database: CartographDatabase,
    project_id: ProjectId,
    generation_id: GenerationId,
    hits: Vec<SourceSearchHit>,
}

async fn attribute_source_hits(
    request: SourceAttributionRequest,
) -> Result<Vec<SourceSearchHit>, SourceSearchError> {
    stream::iter(request.hits)
        .map(|mut hit| {
            let database = request.database.clone();
            let project_id = request.project_id.clone();
            let generation_id = request.generation_id.clone();
            async move {
                let symbols = database
                    .current_symbols_at_range(CurrentSourceRangeLookup::new(
                        CurrentGenerationLookup::new(&project_id, &generation_id),
                        SourceLineRange::new(&hit.path, hit.line, hit.line),
                        ENCLOSING_SYMBOL_LIMIT,
                    ))
                    .await
                    .map_err(map_storage)?;
                hit.enclosing_symbol = symbols
                    .iter()
                    .find(|symbol| {
                        matches!(
                            symbol.symbol_kind(),
                            "function" | "method" | "class" | "interface" | "trait" | "component"
                        )
                    })
                    .or_else(|| symbols.iter().find(|symbol| symbol.symbol_kind() != "file"))
                    .map(|symbol| SourceSearchSymbol {
                        symbol_id: symbol.symbol_id().as_str().to_owned(),
                        qualified_name: symbol.qualified_name().to_owned(),
                        symbol_kind: symbol.symbol_kind().to_owned(),
                        start_line: symbol.start_line(),
                        end_line: symbol.end_line(),
                    });
                Ok::<_, SourceSearchError>(hit)
            }
        })
        .buffered(ATTRIBUTION_CONCURRENCY)
        .try_collect()
        .await
}

#[derive(Clone, Copy)]
struct FileScanRequest<'a> {
    root: &'a Path,
    file: &'a FileSurfaceRow,
    regex: &'a Regex,
    cancellation: &'a ProjectCancellation,
}

fn scan_file(input: FileScanRequest<'_>) -> FileScanResult {
    let FileScanRequest {
        root,
        file,
        regex,
        cancellation,
    } = input;
    if cancellation.is_cancelled() {
        return FileScanResult {
            hits: Vec::new(),
            matched: 0,
            scanned: false,
            skipped_large: false,
            unreadable_or_unsafe: true,
        };
    }
    if file.byte_size() > MAXIMUM_FILE_BYTES {
        return FileScanResult {
            hits: Vec::new(),
            matched: 0,
            scanned: false,
            skipped_large: true,
            unreadable_or_unsafe: false,
        };
    }
    let Ok(path) = NormalizedPath::parse(file.path()) else {
        return unsafe_file();
    };
    let joined = root.join(path.as_str());
    let Some(canonical) = std::fs::canonicalize(&joined)
        .ok()
        .filter(|candidate| candidate.starts_with(root))
    else {
        return unsafe_file();
    };
    if std::fs::metadata(&canonical)
        .ok()
        .is_none_or(|metadata| !metadata.is_file() || metadata.len() > MAXIMUM_FILE_BYTES)
    {
        return unsafe_file();
    }
    let Ok(source) = std::fs::read_to_string(canonical) else {
        return unsafe_file();
    };
    let mut hits = Vec::new();
    let mut matched = 0_u64;
    for (index, line) in source.lines().enumerate() {
        if index % 256 == 0 && cancellation.is_cancelled() {
            break;
        }
        if !regex.is_match(line) {
            continue;
        }
        matched = matched.saturating_add(1);
        if hits.len() >= MAXIMUM_RETAINED_HITS_PER_FILE {
            continue;
        }
        let line_number = u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
        hits.push(SourceSearchHit {
            path: path.clone(),
            language: file.language().to_owned(),
            line: line_number,
            snippet: clipped_snippet(line),
            enclosing_symbol: None,
        });
    }
    FileScanResult {
        hits,
        matched,
        scanned: true,
        skipped_large: false,
        unreadable_or_unsafe: false,
    }
}

fn clipped_snippet(line: &str) -> String {
    let mut characters = line.chars();
    let snippet = characters
        .by_ref()
        .take(MAXIMUM_SNIPPET_CHARACTERS)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{snippet} …")
    } else {
        snippet
    }
}

fn unsafe_file() -> FileScanResult {
    FileScanResult {
        hits: Vec::new(),
        matched: 0,
        scanned: false,
        skipped_large: false,
        unreadable_or_unsafe: true,
    }
}

fn map_storage(_: StorageError) -> SourceSearchError {
    SourceSearchError::StorageUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_search_rejects_unbounded_or_invalid_inputs() {
        assert!(SourceSearchOptions::new("", 10).is_err());
        assert!(SourceSearchOptions::new("x", 0).is_err());
        assert!(SourceSearchOptions::new("x", 501).is_err());
        assert!(SourceSearchOptions::new("x".repeat(1_025), 10).is_err());
    }

    #[test]
    fn invalid_regex_diagnostic_is_precise_and_omits_the_pattern() {
        let options = SourceSearchOptions::new("private_token(foo", 10)
            .unwrap_or_else(|error| panic!("source search options failed: {error}"));
        let error = compile_source_regex(&options)
            .err()
            .unwrap_or_else(|| panic!("invalid regex unexpectedly compiled"));
        assert_eq!(
            error,
            SourceSearchError::InvalidRegex {
                reason: "unclosed group".to_owned(),
                offset: 13,
            }
        );
        assert!(!error.to_string().contains("private_token"));
    }

    #[test]
    fn snippet_clipping_is_unicode_safe() {
        let input = "🙂".repeat(MAXIMUM_SNIPPET_CHARACTERS + 1);
        let clipped = clipped_snippet(&input);
        assert!(clipped.ends_with(" …"));
        assert_eq!(clipped.chars().count(), MAXIMUM_SNIPPET_CHARACTERS + 2);
    }
}
