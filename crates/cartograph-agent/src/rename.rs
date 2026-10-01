use std::collections::{BTreeMap, BTreeSet};

use cartograph_db::{
    CurrentGenerationLookup, CurrentSourceRangeLookup, CurrentSymbolRecord, ProjectSnapshot,
    RenameReferenceSite, SourceLineRange, StorageError,
};
use cartograph_domain::{ContentDigest, NormalizedPath, ProjectId, SourceManifestDigestBuilder};
use cartograph_extract::{
    DiscoveredSource, SourceDiscoveryOptions, SourceReadOptions, SourceRoot, SourceSnapshot,
};
use futures_util::{StreamExt, stream};
use memchr::memchr_iter;
use serde::Serialize;
use thiserror::Error;

use crate::{ProjectCancellation, ProjectRuntime, discovery_limits, utf8_boundary};

const MAXIMUM_RENAME_SITES: u16 = 500;
const MAXIMUM_TEXTUAL_MENTIONS: u16 = 500;
const DEFAULT_MAXIMUM_MENTION_FILE_BYTES: u64 = 1024 * 1024;
const MENTION_SNIPPET_BYTES: usize = 200;
const ENCLOSING_LOOKUP_LIMIT: u16 = 20;
const ENCLOSING_LOOKUP_CONCURRENCY: usize = 8;

/// Validated exact-reference and textual-mention bounds for a rename plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenamePlanOptions {
    reference_limit: u16,
    mention_limit: u16,
    maximum_mention_file_bytes: u64,
}

impl RenamePlanOptions {
    /// Creates validated rename-planning limits.
    ///
    /// # Errors
    ///
    /// Returns [`RenamePlanError::InvalidOptions`] when `reference_limit` is
    /// zero or excessive, or `mention_limit` exceeds its response ceiling.
    pub const fn new(reference_limit: u16, mention_limit: u16) -> Result<Self, RenamePlanError> {
        if reference_limit == 0
            || reference_limit > MAXIMUM_RENAME_SITES
            || mention_limit > MAXIMUM_TEXTUAL_MENTIONS
        {
            return Err(RenamePlanError::InvalidOptions);
        }
        Ok(Self {
            reference_limit,
            mention_limit,
            maximum_mention_file_bytes: DEFAULT_MAXIMUM_MENTION_FILE_BYTES,
        })
    }
}

/// Exact symbol, project, bounds, and cancellation scope for one rename plan.
pub struct RenamePlanRequest {
    /// Stable project ID for this record.
    pub project_id: ProjectId,
    /// Definition for this record.
    pub definition: CurrentSymbolRecord,
    /// Options for this record.
    pub options: RenamePlanOptions,
    /// Cancellation for this record.
    pub cancellation: ProjectCancellation,
}

/// Exact graph reference enriched with a fresh one-based source line.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameReferenceEvidence {
    path: String,
    line: u32,
    owner_symbol_id: Option<String>,
    start_byte: u64,
    end_byte: u64,
    reference_kind: String,
    confidence: f32,
    provenance: String,
    represented_site: String,
}

/// Review-only word-boundary mention outside the declaration/reference rows.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameTextualMention {
    pub(crate) path: String,
    pub(crate) line: u32,
    pub(crate) text: String,
    enclosing_symbol_id: Option<String>,
    pub(crate) enclosing_qualified_name: Option<String>,
    pub(crate) enclosing_symbol_kind: Option<String>,
    confidence: &'static str,
    /// Advisory Jev probability that the mention refers to this symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) decision_probability: Option<f64>,
    /// `likely_other` or `textual_review_required` from
    /// [`Self::decision_probability`], when Jev judged the mention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) triage: Option<&'static str>,
}

/// Complete bounded plan. It never edits source.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePlan {
    pub(crate) definition: CurrentSymbolRecord,
    source_revision: ContentDigest,
    exact_reference_count: u64,
    exact_references_truncated: bool,
    exact_references: Vec<RenameReferenceEvidence>,
    textual_mention_count: u64,
    textual_mentions_truncated: bool,
    pub(crate) textual_mentions: Vec<RenameTextualMention>,
    skipped_large_files: u64,
    edits_applied: bool,
    /// Outcome of the optional Jev mention triage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) decision_triage: Option<crate::decision_rename::RenameTriageEvidence>,
}

/// Credential-safe rename planning failures.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum RenamePlanError {
    #[error("rename plan options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("rename graph evidence is unavailable")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
    #[error("rename source evidence is unavailable")]
    /// Required source evidence could not be read safely.
    SourceUnavailable,
    #[error("source or generation changed during rename planning")]
    /// Live source no longer matches the generation or digest fence.
    SourceChanged,
    #[error("rename planning was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
}

impl ProjectRuntime {
    /// Build a fresh exact-reference plus textual-mention plan without editing files.
    /// # Errors
    ///
    /// Returns an error when the definition does not belong to the fenced
    /// current generation, graph or live source evidence is unavailable,
    /// source/generation identity changes, or cancellation wins.
    pub async fn plan_rename(
        &self,
        input: RenamePlanRequest,
    ) -> Result<RenamePlan, RenamePlanError> {
        let RenamePlanRequest {
            project_id,
            definition,
            options,
            cancellation,
        } = input;
        let before = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| RenamePlanError::StorageUnavailable)?
            .ok_or(RenamePlanError::SourceChanged)?;
        let generation_fence = rename_generation(&before, &project_id, &definition)?;
        let references = self
            .database()
            .current_rename_reference_evidence(&project_id, definition.symbol_id())
            .await
            .map_err(|_| RenamePlanError::StorageUnavailable)?;
        let exact_reference_count = u64::try_from(references.len()).unwrap_or(u64::MAX);
        let permit = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(RenamePlanError::Cancelled),
            result = self.source_scan_permits.clone().acquire_owned() => {
                result.map_err(|_| RenamePlanError::SourceUnavailable)?
            }
        };
        let root = self.root.clone();
        let definition_path = definition.path().as_str().to_owned();
        let definition_line = definition.start_line();
        let symbol_name = simple_symbol_name(definition.qualified_name()).to_owned();
        let worker_cancellation = cancellation.clone();
        let mut scan = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            scan_rename_source(
                RenameSourceRequest {
                    root: &root,
                    symbol_name: &symbol_name,
                    definition_path: &definition_path,
                    definition_line,
                    references,
                    options,
                },
                || worker_cancellation.is_cancelled(),
            )
        })
        .await
        .map_err(|_| RenamePlanError::SourceUnavailable)??;
        if cancellation.is_cancelled() {
            return Err(RenamePlanError::Cancelled);
        }
        let observed = self
            .scan_source(None, cancellation.clone())
            .await
            .map_err(|_| RenamePlanError::SourceUnavailable)?;
        if observed.digest.as_str() != generation_fence.source_revision {
            return Err(RenamePlanError::SourceChanged);
        }
        scan.source_revision = observed.digest;
        let mentions = attach_enclosing_symbols(EnclosingSymbolRequest {
            runtime: self,
            project_id: &generation_fence.project_id,
            generation_id: &generation_fence.generation_id,
            mentions: scan.mentions,
            cancellation: cancellation.clone(),
        })
        .await?;
        let after = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| RenamePlanError::StorageUnavailable)?;
        if !rename_snapshot_matches(after.as_ref(), &generation_fence) {
            return Err(RenamePlanError::SourceChanged);
        }
        let mut exact_references = scan.references;
        exact_references.truncate(usize::from(options.reference_limit));
        Ok(RenamePlan {
            definition,
            source_revision: scan.source_revision,
            exact_reference_count,
            exact_references_truncated: exact_reference_count > u64::from(options.reference_limit),
            exact_references,
            textual_mention_count: scan.mention_count,
            textual_mentions_truncated: scan.mention_count > u64::from(options.mention_limit),
            textual_mentions: mentions,
            skipped_large_files: scan.skipped_large_files,
            edits_applied: false,
            decision_triage: None,
        })
    }
}

struct RenameGenerationFence {
    project_id: ProjectId,
    generation_id: cartograph_domain::GenerationId,
    source_revision: String,
}

fn rename_snapshot_matches(
    snapshot: Option<&ProjectSnapshot>,
    fence: &RenameGenerationFence,
) -> bool {
    snapshot.is_some_and(|snapshot| {
        snapshot.project_id == fence.project_id
            && snapshot.current.as_ref().is_some_and(|current| {
                current.generation_id == fence.generation_id
                    && current.source_revision == fence.source_revision
            })
    })
}

fn rename_generation(
    snapshot: &ProjectSnapshot,
    project_id: &ProjectId,
    definition: &CurrentSymbolRecord,
) -> Result<RenameGenerationFence, RenamePlanError> {
    let current = snapshot
        .current
        .as_ref()
        .ok_or(RenamePlanError::SourceChanged)?;
    if snapshot.project_id != *project_id || definition.generation_id() != &current.generation_id {
        return Err(RenamePlanError::SourceChanged);
    }
    Ok(RenameGenerationFence {
        project_id: project_id.clone(),
        generation_id: current.generation_id.clone(),
        source_revision: current.source_revision.clone(),
    })
}

struct RenameSourceScan {
    source_revision: ContentDigest,
    references: Vec<RenameReferenceEvidence>,
    mentions: Vec<RawMention>,
    mention_count: u64,
    skipped_large_files: u64,
}

struct RenameSourceRequest<'value> {
    root: &'value std::path::Path,
    symbol_name: &'value str,
    definition_path: &'value str,
    definition_line: u32,
    references: Vec<RenameReferenceSite>,
    options: RenamePlanOptions,
}

#[derive(Clone)]
struct RawMention {
    path: String,
    line: u32,
    text: String,
}

fn scan_rename_source<Cancel>(
    request: RenameSourceRequest<'_>,
    mut cancelled: Cancel,
) -> Result<RenameSourceScan, RenamePlanError>
where
    Cancel: FnMut() -> bool,
{
    let RenameSourceRequest {
        root,
        symbol_name,
        definition_path,
        definition_line,
        references,
        options,
    } = request;
    let source_policy =
        crate::project_source_policy(root).map_err(|_| RenamePlanError::SourceUnavailable)?;
    let maximum_source_bytes = source_policy
        .maximum_file_bytes
        .unwrap_or(crate::DEFAULT_MAX_SOURCE_BYTES);
    let source_root = SourceRoot::open_with_policy(root, source_policy.discovery)
        .map_err(|_| RenamePlanError::SourceUnavailable)?;
    let discovery = discovery_limits().map_err(|_| RenamePlanError::SourceUnavailable)?;
    let read_limits = crate::source_limits_with_max(maximum_source_bytes)
        .map_err(|_| RenamePlanError::SourceUnavailable)?;
    let files = source_root
        .discover_with_cancellation(SourceDiscoveryOptions::new(discovery, &mut cancelled))
        .map_err(map_source_error)?;
    let mut state = RenameScanAccumulator::new(references, files.len())?;
    for file in &files {
        if cancelled() {
            return Err(RenamePlanError::Cancelled);
        }
        let snapshot = source_root
            .read_with_cancellation(
                file.path(),
                SourceReadOptions::new(read_limits, &mut cancelled),
            )
            .map_err(map_source_error)?;
        state
            .digest
            .push(file.path(), snapshot.content_hash())
            .map_err(|_| RenamePlanError::SourceUnavailable)?;
        scan_rename_file(
            &mut state,
            RenameFileScan {
                file,
                snapshot: &snapshot,
                symbol_name,
                definition_path,
                definition_line,
                options,
            },
        )?;
    }
    if !state.references_by_path.is_empty() {
        return Err(RenamePlanError::SourceChanged);
    }
    Ok(RenameSourceScan {
        source_revision: state
            .digest
            .finish()
            .map_err(|_| RenamePlanError::SourceUnavailable)?,
        references: state.exact,
        mentions: state.mentions,
        mention_count: state.mention_count,
        skipped_large_files: state.skipped_large_files,
    })
}

struct RenameScanAccumulator {
    references_by_path: BTreeMap<String, Vec<RenameReferenceSite>>,
    digest: SourceManifestDigestBuilder,
    exact: Vec<RenameReferenceEvidence>,
    mentions: Vec<RawMention>,
    mention_count: u64,
    skipped_large_files: u64,
}

impl RenameScanAccumulator {
    fn new(
        references: Vec<RenameReferenceSite>,
        file_count: usize,
    ) -> Result<Self, RenamePlanError> {
        let mut references_by_path = BTreeMap::<String, Vec<RenameReferenceSite>>::new();
        for reference in references {
            references_by_path
                .entry(reference.path().to_owned())
                .or_default()
                .push(reference);
        }
        Ok(Self {
            references_by_path,
            digest: SourceManifestDigestBuilder::new(file_count)
                .map_err(|_| RenamePlanError::SourceUnavailable)?,
            exact: Vec::new(),
            mentions: Vec::new(),
            mention_count: 0,
            skipped_large_files: 0,
        })
    }
}

#[derive(Clone, Copy)]
struct RenameFileScan<'source> {
    file: &'source DiscoveredSource,
    snapshot: &'source SourceSnapshot,
    symbol_name: &'source str,
    definition_path: &'source str,
    definition_line: u32,
    options: RenamePlanOptions,
}

fn scan_rename_file(
    state: &mut RenameScanAccumulator,
    input: RenameFileScan<'_>,
) -> Result<(), RenamePlanError> {
    let exact_lines = collect_exact_rename_references(state, &input)?;
    if input.options.mention_limit == 0
        || input.file.byte_size() > input.options.maximum_mention_file_bytes
    {
        if input.file.byte_size() > input.options.maximum_mention_file_bytes {
            state.skipped_large_files = state.skipped_large_files.saturating_add(1);
        }
        return Ok(());
    }
    collect_textual_rename_mentions(state, &input, &exact_lines)
}

fn collect_exact_rename_references(
    state: &mut RenameScanAccumulator,
    input: &RenameFileScan<'_>,
) -> Result<BTreeSet<u32>, RenamePlanError> {
    let mut exact_lines = BTreeSet::new();
    let Some(file_references) = state.references_by_path.remove(input.file.path().as_str()) else {
        return Ok(exact_lines);
    };
    for reference in file_references {
        let start =
            usize::try_from(reference.start_byte()).map_err(|_| RenamePlanError::SourceChanged)?;
        let end =
            usize::try_from(reference.end_byte()).map_err(|_| RenamePlanError::SourceChanged)?;
        let represented_site = input
            .snapshot
            .source()
            .get(start..end)
            .ok_or(RenamePlanError::SourceChanged)?;
        let line = one_based_line(input.snapshot.source(), start)?;
        exact_lines.insert(line);
        state.exact.push(RenameReferenceEvidence {
            path: input.file.path().as_str().to_owned(),
            line,
            owner_symbol_id: reference.owner_symbol_id().map(str::to_owned),
            start_byte: reference.start_byte(),
            end_byte: reference.end_byte(),
            reference_kind: reference.reference_kind().to_owned(),
            confidence: reference.confidence(),
            provenance: reference.provenance().to_owned(),
            represented_site: bounded_text(represented_site.trim(), MENTION_SNIPPET_BYTES),
        });
    }
    Ok(exact_lines)
}

fn collect_textual_rename_mentions(
    state: &mut RenameScanAccumulator,
    input: &RenameFileScan<'_>,
    exact_lines: &BTreeSet<u32>,
) -> Result<(), RenamePlanError> {
    for (index, line) in input.snapshot.source().lines().enumerate() {
        if !line_has_identifier(line, input.symbol_name) {
            continue;
        }
        let line_number = u32::try_from(index)
            .ok()
            .and_then(|line| line.checked_add(1))
            .ok_or(RenamePlanError::SourceUnavailable)?;
        let is_definition = input.file.path().as_str() == input.definition_path
            && line_number == input.definition_line;
        if is_definition || exact_lines.contains(&line_number) {
            continue;
        }
        state.mention_count = state.mention_count.saturating_add(1);
        if state.mentions.len() < usize::from(input.options.mention_limit) {
            state.mentions.push(RawMention {
                path: input.file.path().as_str().to_owned(),
                line: line_number,
                text: bounded_text(line.trim(), MENTION_SNIPPET_BYTES),
            });
        }
    }
    Ok(())
}

struct EnclosingSymbolRequest<'a> {
    runtime: &'a ProjectRuntime,
    project_id: &'a ProjectId,
    generation_id: &'a cartograph_domain::GenerationId,
    mentions: Vec<RawMention>,
    cancellation: ProjectCancellation,
}

async fn attach_enclosing_symbols(
    input: EnclosingSymbolRequest<'_>,
) -> Result<Vec<RenameTextualMention>, RenamePlanError> {
    let EnclosingSymbolRequest {
        runtime,
        project_id,
        generation_id,
        mentions,
        cancellation,
    } = input;
    let database = runtime.database().clone();
    let project_id = project_id.clone();
    let generation_id = generation_id.clone();
    let mut resolved = stream::iter(mentions.into_iter().enumerate())
        .map(|(index, mention)| {
            let database = database.clone();
            let project_id = project_id.clone();
            let generation_id = generation_id.clone();
            let cancellation = cancellation.clone();
            async move {
                if cancellation.is_cancelled() {
                    return Err(RenamePlanError::Cancelled);
                }
                let path = NormalizedPath::parse(&mention.path)
                    .map_err(|_| RenamePlanError::SourceChanged)?;
                let symbols = database
                    .current_symbols_at_range(CurrentSourceRangeLookup::new(
                        CurrentGenerationLookup::new(&project_id, &generation_id),
                        SourceLineRange::new(&path, mention.line, mention.line),
                        ENCLOSING_LOOKUP_LIMIT,
                    ))
                    .await
                    .map_err(|error| map_storage_error(&error))?;
                let enclosing = symbols
                    .iter()
                    .filter(|symbol| {
                        matches!(
                            symbol.symbol_kind(),
                            "function" | "method" | "class" | "interface" | "component"
                        )
                    })
                    .min_by_key(|symbol| symbol.end_line().saturating_sub(symbol.start_line()));
                Ok((
                    index,
                    RenameTextualMention {
                        path: mention.path,
                        line: mention.line,
                        text: mention.text,
                        enclosing_symbol_id: enclosing
                            .map(|symbol| symbol.symbol_id().as_str().to_owned()),
                        enclosing_qualified_name: enclosing
                            .map(|symbol| symbol.qualified_name().to_owned()),
                        enclosing_symbol_kind: enclosing
                            .map(|symbol| symbol.symbol_kind().to_owned()),
                        confidence: "textual_review_required",
                        decision_probability: None,
                        triage: None,
                    },
                ))
            }
        })
        .buffer_unordered(ENCLOSING_LOOKUP_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, RenamePlanError>>()?;
    resolved.sort_by_key(|(index, _)| *index);
    Ok(resolved.into_iter().map(|(_, mention)| mention).collect())
}

fn simple_symbol_name(qualified_name: &str) -> &str {
    let last = qualified_name
        .rsplit(['.', '#', '/'])
        .next()
        .unwrap_or(qualified_name);
    last.rsplit_once("::").map_or(last, |(_, name)| name)
}

fn line_has_identifier(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    line.match_indices(name).any(|(start, matched)| {
        let before = line[..start].chars().next_back();
        let after = line[start + matched.len()..].chars().next();
        before.is_none_or(|character| !identifier_character(character))
            && after.is_none_or(|character| !identifier_character(character))
    })
}

fn identifier_character(character: char) -> bool {
    character == '_' || character == '$' || character.is_alphanumeric()
}

fn one_based_line(source: &str, start: usize) -> Result<u32, RenamePlanError> {
    let prefix = source
        .as_bytes()
        .get(..start)
        .ok_or(RenamePlanError::SourceChanged)?;
    u32::try_from(memchr_iter(b'\n', prefix).count())
        .ok()
        .and_then(|line| line.checked_add(1))
        .ok_or(RenamePlanError::SourceUnavailable)
}

fn bounded_text(value: &str, maximum: usize) -> String {
    let boundary = utf8_boundary(value, maximum);
    if boundary < value.len() {
        format!("{} …", &value[..boundary])
    } else {
        value.to_owned()
    }
}

fn map_storage_error(error: &StorageError) -> RenamePlanError {
    match error {
        StorageError::CurrentGenerationChanged => RenamePlanError::SourceChanged,
        _ => RenamePlanError::StorageUnavailable,
    }
}

fn map_source_error<Error>(_error: Error) -> RenamePlanError {
    RenamePlanError::SourceUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_boundaries_support_unicode_and_dollar_names() {
        assert!(line_has_identifier("call Élément now", "Élément"));
        assert!(line_has_identifier("const value = data$;", "data$"));
        assert!(!line_has_identifier("database", "data"));
        assert!(!line_has_identifier("data2", "data"));
    }

    #[test]
    fn simple_names_handle_common_qualified_separators() {
        assert_eq!(simple_symbol_name("crate::module::parse"), "parse");
        assert_eq!(simple_symbol_name("Class.method"), "method");
        assert_eq!(simple_symbol_name("plain"), "plain");
    }
}
