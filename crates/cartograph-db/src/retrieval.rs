use std::time::Duration;

use cartograph_domain::{
    ContentDigest, FileId, GenerationId, NormalizedPath, ProjectId, SourceLanguage,
    SymbolExecutionFlags, SymbolExportFlags, SymbolId, Visibility,
};
use serde::Serialize;

pub(crate) mod native;

const MAX_EXACT_TEXT_BYTES: usize = 4_096;
const MAX_LOOKUP_LIMIT: u16 = 500;
const MAX_FILE_LIST_LIMIT: u16 = 2_000;
const MAX_ENTRY_POINT_LIMIT: u16 = 200;
const MAX_FRONTIER_SYMBOLS: usize = 500;
const MAX_EDGE_LIMIT: u16 = 2_000;
const DEFAULT_INTERACTIVE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Identity of the immutable generation currently published for a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentGenerationRecord {
    generation_id: GenerationId,
    sequence: u64,
}

impl CurrentGenerationRecord {
    /// Published generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Monotonic generation sequence within the project.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Immutable project/generation fence shared by current-generation lookups.
#[derive(Clone, Copy, Debug)]
pub struct CurrentGenerationLookup<'lookup> {
    project_id: &'lookup ProjectId,
    expected_generation_id: &'lookup GenerationId,
}

impl<'lookup> CurrentGenerationLookup<'lookup> {
    /// Bind one project to the exact generation callers already observed.
    #[must_use]
    pub const fn new(
        project_id: &'lookup ProjectId,
        expected_generation_id: &'lookup GenerationId,
    ) -> Self {
        Self {
            project_id,
            expected_generation_id,
        }
    }

    /// Project constrained by this immutable-generation fence.
    #[must_use]
    pub const fn project_id(&self) -> &'lookup ProjectId {
        self.project_id
    }

    /// Published generation callers already observed for the project.
    #[must_use]
    pub const fn expected_generation_id(&self) -> &'lookup GenerationId {
        self.expected_generation_id
    }
}

/// One file from the current published generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentFileRecord {
    generation_id: GenerationId,
    file_id: FileId,
    path: NormalizedPath,
    language: String,
    content_hash: ContentDigest,
}

impl CurrentFileRecord {
    /// Published generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Stable file identity.
    #[must_use]
    pub const fn file_id(&self) -> &FileId {
        &self.file_id
    }

    /// Canonical project-relative path.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Stable indexed language name.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Exact source digest captured by the published generation.
    #[must_use]
    pub const fn content_hash(&self) -> &ContentDigest {
        &self.content_hash
    }
}

/// One symbol from the current published generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentSymbolRecord {
    generation_id: GenerationId,
    symbol_id: SymbolId,
    file_id: FileId,
    path: NormalizedPath,
    language: String,
    symbol_kind: String,
    qualified_name: String,
    signature: String,
    start_line: u32,
    end_line: u32,
    visibility: Option<Visibility>,
    #[serde(flatten)]
    export: SymbolExportFlags,
    #[serde(flatten)]
    execution: SymbolExecutionFlags,
    declaration_only: bool,
}

impl CurrentSymbolRecord {
    /// Published generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Stable symbol identity.
    #[must_use]
    pub const fn symbol_id(&self) -> &SymbolId {
        &self.symbol_id
    }

    /// Stable file identity.
    #[must_use]
    pub const fn file_id(&self) -> &FileId {
        &self.file_id
    }

    /// Canonical project-relative path.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Stable indexed language name.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Stable symbol-kind name.
    #[must_use]
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    /// Fully qualified declaration name.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Source signature captured by the extractor.
    #[must_use]
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// One-based first source line.
    #[must_use]
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    /// One-based last source line.
    #[must_use]
    pub const fn end_line(&self) -> u32 {
        self.end_line
    }

    /// Language-level declaration visibility when statically known.
    #[must_use]
    pub const fn visibility(&self) -> Option<Visibility> {
        self.visibility
    }

    /// Whether the declaration is exported from its module or package.
    #[must_use]
    pub const fn exported(&self) -> bool {
        self.export.exported
    }

    /// Whether the declaration is the module's default export.
    #[must_use]
    pub const fn default_export(&self) -> bool {
        self.export.default_export
    }

    /// Whether the declaration is asynchronous.
    #[must_use]
    pub const fn async_symbol(&self) -> bool {
        self.execution.async_symbol
    }

    /// Whether the declaration is a static member.
    #[must_use]
    pub const fn static_member(&self) -> bool {
        self.execution.static_member
    }

    /// Whether the declaration lacks an implementation body.
    #[must_use]
    pub const fn declaration_only(&self) -> bool {
        self.declaration_only
    }
}

/// Exact unresolved or resolved reference evidence from the current generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CurrentReferenceRecord {
    reference_id: u64,
    generation_id: GenerationId,
    file_id: FileId,
    path: NormalizedPath,
    owner_symbol_id: Option<SymbolId>,
    target_symbol_id: Option<SymbolId>,
    reference_name: String,
    reference_kind: String,
    start_byte: u64,
    end_byte: u64,
    confidence: f32,
    provenance: String,
    site_count: u32,
    span_precision: ReferenceSpanPrecision,
}

impl CurrentReferenceRecord {
    /// Stable row identity within the configured schema.
    #[must_use]
    pub const fn reference_id(&self) -> u64 {
        self.reference_id
    }

    /// Published generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// File containing the reference.
    #[must_use]
    pub const fn file_id(&self) -> &FileId {
        &self.file_id
    }

    /// Canonical path containing the reference.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Symbol whose body owns the reference, when known.
    #[must_use]
    pub const fn owner_symbol_id(&self) -> Option<&SymbolId> {
        self.owner_symbol_id.as_ref()
    }

    /// Resolved target symbol, when known.
    #[must_use]
    pub const fn target_symbol_id(&self) -> Option<&SymbolId> {
        self.target_symbol_id.as_ref()
    }

    /// Retained normalized reference name; coarse legacy anchors need not contain it literally.
    #[must_use]
    pub fn reference_name(&self) -> &str {
        &self.reference_name
    }

    /// Stable reference-kind name.
    #[must_use]
    pub fn reference_kind(&self) -> &str {
        &self.reference_kind
    }

    /// Inclusive source byte offset.
    #[must_use]
    pub const fn start_byte(&self) -> u64 {
        self.start_byte
    }

    /// Exclusive source byte offset.
    #[must_use]
    pub const fn end_byte(&self) -> u64 {
        self.end_byte
    }

    /// Extractor confidence in the reference resolution.
    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    /// Stable extractor provenance label.
    #[must_use]
    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    /// Exact number of source sites represented by this retained anchor.
    #[must_use]
    pub const fn site_count(&self) -> u32 {
        self.site_count
    }

    /// Whether the span is an exact token or a bounded coarse legacy anchor.
    #[must_use]
    pub const fn span_precision(&self) -> ReferenceSpanPrecision {
        self.span_precision
    }
}

/// Direction in which a bounded graph frontier is expanded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphDirection {
    /// Follow source symbols to their targets.
    Outgoing,
    /// Follow target symbols back to their sources.
    Incoming,
}

/// Stable top-of-stack category used by entry-point discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryPointBucket {
    /// Framework-resolved HTTP or RPC routes.
    Routes,
    /// Framework-resolved CLI subcommands.
    CliCommands,
    /// Exported MCP tool-definition constants.
    McpTools,
    /// Exported declarations under conventional CLI source directories.
    CliFiles,
    /// Exported API declarations with no in-tree structural consumer.
    PublicExports,
}

impl EntryPointBucket {
    /// Complete stable bucket order used by human and machine consumers.
    pub const ALL: [Self; 5] = [
        Self::Routes,
        Self::CliCommands,
        Self::McpTools,
        Self::CliFiles,
        Self::PublicExports,
    ];

    /// Stable CLI/MCP representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Routes => "routes",
            Self::CliCommands => "cli_commands",
            Self::McpTools => "mcp_tools",
            Self::CliFiles => "cli_files",
            Self::PublicExports => "public_exports",
        }
    }

    /// Parse a stable CLI/MCP representation.
    #[must_use]
    pub fn from_stable_str(value: &str) -> Option<Self> {
        match value {
            "routes" => Some(Self::Routes),
            "cli" | "cli_commands" => Some(Self::CliCommands),
            "mcp_tools" => Some(Self::McpTools),
            "cli_files" => Some(Self::CliFiles),
            "public_exports" => Some(Self::PublicExports),
            _ => None,
        }
    }
}

/// One bounded entry-point bucket with its exact pre-limit cardinality.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentEntryPointPage {
    bucket: EntryPointBucket,
    total: u64,
    truncated: bool,
    symbols: Vec<CurrentSymbolRecord>,
}

impl CurrentEntryPointPage {
    /// Stable category represented by this page.
    #[must_use]
    pub const fn bucket(&self) -> EntryPointBucket {
        self.bucket
    }

    /// Exact number of matching current-generation symbols before the limit.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Whether the bounded page omits matching symbols.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Stable source-ordered symbols in this page.
    #[must_use]
    pub fn symbols(&self) -> &[CurrentSymbolRecord] {
        &self.symbols
    }
}

/// Bounded current-generation entry-point lookup for one exact bucket.
#[derive(Clone, Copy, Debug)]
pub struct CurrentEntryPointsLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    bucket: EntryPointBucket,
    limit: u16,
}

impl<'a> CurrentEntryPointsLookup<'a> {
    /// Bind one entry-point category to one immutable current generation.
    #[must_use]
    pub const fn new(
        generation: CurrentGenerationLookup<'a>,
        bucket: EntryPointBucket,
        limit: u16,
    ) -> Self {
        Self {
            project_id: generation.project_id,
            expected_generation_id: generation.expected_generation_id,
            bucket,
            limit,
        }
    }
}

/// Exact current-generation file lookup.
#[derive(Clone, Copy, Debug)]
pub struct CurrentFileLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    path: &'a NormalizedPath,
}

/// Bounded current-generation file inventory with optional exact directory and language filters.
#[derive(Clone, Copy, Debug)]
pub struct CurrentFilesLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    directory: Option<&'a NormalizedPath>,
    language: Option<SourceLanguage>,
    limit: u16,
}

impl<'a> CurrentFilesLookup<'a> {
    /// Bind a bounded file inventory to one immutable current generation.
    #[must_use]
    pub const fn new(
        project_id: &'a ProjectId,
        expected_generation_id: &'a GenerationId,
        limit: u16,
    ) -> Self {
        Self {
            project_id,
            expected_generation_id,
            directory: None,
            language: None,
            limit,
        }
    }

    /// Restrict results to this directory or one of its descendants.
    #[must_use]
    pub const fn within_directory(mut self, directory: &'a NormalizedPath) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Restrict results to one stable language identifier.
    #[must_use]
    pub const fn with_language(mut self, language: SourceLanguage) -> Self {
        self.language = Some(language);
        self
    }
}

impl<'a> CurrentFileLookup<'a> {
    /// Bind one canonical path to its project.
    #[must_use]
    pub const fn new(
        project_id: &'a ProjectId,
        expected_generation_id: &'a GenerationId,
        path: &'a NormalizedPath,
    ) -> Self {
        Self {
            project_id,
            expected_generation_id,
            path,
        }
    }
}

/// Exact current-generation text lookup with a bounded result count.
#[derive(Clone, Copy, Debug)]
pub struct ExactTextLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    text: &'a str,
    limit: u16,
}

impl<'a> ExactTextLookup<'a> {
    /// Bind exact text and its result limit to a project.
    #[must_use]
    pub const fn new(generation: CurrentGenerationLookup<'a>, text: &'a str, limit: u16) -> Self {
        Self {
            project_id: generation.project_id,
            expected_generation_id: generation.expected_generation_id,
            text,
            limit,
        }
    }
}

/// Bounded symbol lookup for one current-generation file.
#[derive(Clone, Copy, Debug)]
pub struct CurrentFileSymbolsLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    file_id: &'a FileId,
    limit: u16,
}

/// Bounded symbol lookup overlapping one exact one-based source-line range.
#[derive(Clone, Copy, Debug)]
pub struct CurrentSourceRangeLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    path: &'a NormalizedPath,
    start_line: u32,
    end_line: u32,
    limit: u16,
}

/// Inclusive one-based line range within one canonical path.
#[derive(Clone, Copy, Debug)]
pub struct SourceLineRange<'range> {
    path: &'range NormalizedPath,
    start_line: u32,
    end_line: u32,
}

impl<'range> SourceLineRange<'range> {
    /// Bind one canonical path to an inclusive source-line range.
    #[must_use]
    pub const fn new(path: &'range NormalizedPath, start_line: u32, end_line: u32) -> Self {
        Self {
            path,
            start_line,
            end_line,
        }
    }
}

impl<'a> CurrentSourceRangeLookup<'a> {
    /// Bind an inclusive source range and result limit to one immutable generation.
    #[must_use]
    pub const fn new(
        generation: CurrentGenerationLookup<'a>,
        range: SourceLineRange<'a>,
        limit: u16,
    ) -> Self {
        Self {
            project_id: generation.project_id,
            expected_generation_id: generation.expected_generation_id,
            path: range.path,
            start_line: range.start_line,
            end_line: range.end_line,
            limit,
        }
    }
}

impl<'a> CurrentFileSymbolsLookup<'a> {
    /// Bind one file and its result limit to a project.
    #[must_use]
    pub const fn new(
        generation: CurrentGenerationLookup<'a>,
        file_id: &'a FileId,
        limit: u16,
    ) -> Self {
        Self {
            project_id: generation.project_id,
            expected_generation_id: generation.expected_generation_id,
            file_id,
            limit,
        }
    }
}

/// Bounded current-generation hydration for a set of symbol identities.
#[derive(Clone, Copy, Debug)]
pub struct CurrentSymbolSetLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    symbol_ids: &'a [SymbolId],
}

impl<'a> CurrentSymbolSetLookup<'a> {
    /// Bind symbol identities to a project.
    #[must_use]
    pub const fn new(
        project_id: &'a ProjectId,
        expected_generation_id: &'a GenerationId,
        symbol_ids: &'a [SymbolId],
    ) -> Self {
        Self {
            project_id,
            expected_generation_id,
            symbol_ids,
        }
    }
}

/// Bounded current-generation graph frontier lookup.
#[derive(Clone, Copy, Debug)]
pub struct CurrentGraphLookup<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    frontier: &'a [SymbolId],
    direction: GraphDirection,
    limit: u16,
    include_test_targets: bool,
}

impl<'a> CurrentGraphLookup<'a> {
    /// Bind a graph frontier and direction to a project.
    #[must_use]
    pub const fn new(
        generation: CurrentGenerationLookup<'a>,
        frontier: &'a [SymbolId],
        direction: GraphDirection,
    ) -> Self {
        Self {
            project_id: generation.project_id,
            expected_generation_id: generation.expected_generation_id,
            frontier,
            direction,
            limit: MAX_EDGE_LIMIT,
            include_test_targets: true,
        }
    }

    /// Override the default maximum edge count.
    #[must_use]
    pub const fn with_limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }

    /// Decide whether conventional test-path symbols may enter this frontier.
    #[must_use]
    pub const fn with_test_targets(mut self, include: bool) -> Self {
        self.include_test_targets = include;
        self
    }
}

/// One structural edge from the current published generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CurrentGraphEdge {
    generation_id: GenerationId,
    source_symbol_id: SymbolId,
    target_symbol_id: SymbolId,
    edge_kind: String,
    confidence: f32,
    provenance: String,
    site_count: u32,
}

impl CurrentGraphEdge {
    /// Published generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Edge source.
    #[must_use]
    pub const fn source_symbol_id(&self) -> &SymbolId {
        &self.source_symbol_id
    }

    /// Edge target.
    #[must_use]
    pub const fn target_symbol_id(&self) -> &SymbolId {
        &self.target_symbol_id
    }

    /// Stable structural relation name.
    #[must_use]
    pub fn edge_kind(&self) -> &str {
        &self.edge_kind
    }

    /// Extractor confidence for the relation.
    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    /// Stable extractor provenance label.
    #[must_use]
    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    /// Exact number of source relation sites represented by this edge.
    #[must_use]
    pub const fn site_count(&self) -> u32 {
        self.site_count
    }
}

fn validate_exact_text<'a>(value: &'a str, field: &'static str) -> Result<&'a str, StorageError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_EXACT_TEXT_BYTES || value.contains('\0') {
        return Err(invalid(field));
    }
    Ok(value)
}

const fn validate_limit(limit: u16, maximum: u16) -> Result<(), StorageError> {
    if limit == 0 || limit > maximum {
        return Err(invalid("limit"));
    }
    Ok(())
}

fn validate_symbol_set(symbol_ids: &[SymbolId]) -> Result<(), StorageError> {
    if symbol_ids.len() > MAX_FRONTIER_SYMBOLS {
        return Err(invalid("symbol_ids"));
    }
    Ok(())
}

const fn invalid(field: &'static str) -> StorageError {
    StorageError::InvalidInput { field }
}

use crate::{ReferenceSpanPrecision, StorageError};

fn validate_source_range(start: u32, end: u32) -> Result<(), StorageError> {
    if start == 0 || end < start {
        Err(StorageError::InvalidInput {
            field: "source_range",
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_text_and_graph_bounds_are_fail_closed() {
        assert_eq!(
            validate_exact_text(" ", "name"),
            Err(StorageError::InvalidInput { field: "name" })
        );
        assert_eq!(
            validate_exact_text(&"x".repeat(MAX_EXACT_TEXT_BYTES + 1), "name"),
            Err(StorageError::InvalidInput { field: "name" })
        );
        assert_eq!(
            validate_limit(0, MAX_LOOKUP_LIMIT),
            Err(StorageError::InvalidInput { field: "limit" })
        );
        assert_eq!(
            validate_limit(MAX_EDGE_LIMIT + 1, MAX_EDGE_LIMIT),
            Err(StorageError::InvalidInput { field: "limit" })
        );
        assert_eq!(
            validate_source_range(0, 1),
            Err(StorageError::InvalidInput {
                field: "source_range"
            })
        );
        assert_eq!(
            validate_source_range(3, 2),
            Err(StorageError::InvalidInput {
                field: "source_range"
            })
        );
        assert_eq!(validate_source_range(2, 2), Ok(()));
    }

    #[test]
    fn public_validation_errors_do_not_render_input_values() {
        let untrusted_input = "opaque-caller-input";
        let error = validate_exact_text(&format!("{untrusted_input}\0"), "reference_name")
            .err()
            .unwrap_or(StorageError::InvalidInput {
                field: "test-fixture",
            });
        let rendered = error.to_string();
        assert!(!rendered.contains(untrusted_input));
    }
}
