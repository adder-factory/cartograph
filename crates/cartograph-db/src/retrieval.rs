use std::time::Duration;

use cartograph_domain::{
    ContentDigest, FileId, GenerationId, NormalizedPath, ProjectId, SourceLanguage,
    SymbolExecutionFlags, SymbolExportFlags, SymbolId, Visibility,
};
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

use crate::{
    CartographDatabase, ReferenceSpanPrecision, StorageError,
    database::{
        parse_stored_generation_id, read_stored_bool as read_bool, read_stored_string,
        stored_value_error,
    },
};

const MAX_EXACT_TEXT_BYTES: usize = 4_096;
const MAX_LOOKUP_LIMIT: u16 = 500;
const MAX_FILE_LIST_LIMIT: u16 = 2_000;
const MAX_ENTRY_POINT_LIMIT: u16 = 200;
const MAX_FRONTIER_SYMBOLS: usize = 500;
const MAX_EDGE_LIMIT: u16 = 2_000;
const DEFAULT_INTERACTIVE_READ_TIMEOUT: Duration = Duration::from_secs(30);
const SYMBOL_PATH_COLUMN: usize = 3;
const SYMBOL_LANGUAGE_COLUMN: usize = 4;
const SYMBOL_KIND_COLUMN: usize = 5;
const SYMBOL_QUALIFIED_NAME_COLUMN: usize = 6;
const SYMBOL_SIGNATURE_COLUMN: usize = 7;
const SYMBOL_START_LINE_COLUMN: usize = 8;
const SYMBOL_END_LINE_COLUMN: usize = 9;
const SYMBOL_VISIBILITY_COLUMN: usize = 10;
const SYMBOL_EXPORTED_COLUMN: usize = 11;
const SYMBOL_DEFAULT_EXPORT_COLUMN: usize = 12;
const SYMBOL_ASYNC_COLUMN: usize = 13;
const SYMBOL_STATIC_COLUMN: usize = 14;
const SYMBOL_DECLARATION_ONLY_COLUMN: usize = 15;
const REFERENCE_PATH_COLUMN: usize = 3;
const REFERENCE_OWNER_COLUMN: usize = 4;
const REFERENCE_TARGET_COLUMN: usize = 5;
const REFERENCE_NAME_COLUMN: usize = 6;
const REFERENCE_KIND_COLUMN: usize = 7;
const REFERENCE_START_BYTE_COLUMN: usize = 8;
const REFERENCE_END_BYTE_COLUMN: usize = 9;
const REFERENCE_CONFIDENCE_COLUMN: usize = 10;
const REFERENCE_PROVENANCE_COLUMN: usize = 11;
const REFERENCE_SITE_COUNT_COLUMN: usize = 12;
const REFERENCE_SPAN_PRECISION_COLUMN: usize = 13;
const EDGE_KIND_COLUMN: usize = 3;
const EDGE_CONFIDENCE_COLUMN: usize = 4;
const EDGE_PROVENANCE_COLUMN: usize = 5;
const EDGE_SITE_COUNT_COLUMN: usize = 6;

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

struct ExactRowsQuery<'a> {
    request: ExactTextLookup<'a>,
    statement_timeout: Duration,
    sql: String,
    validation_field: &'static str,
    operation: &'static str,
    commit_operation: &'static str,
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

impl CartographDatabase {
    /// Resolve the current generation pointer without observing staging rows.
    /// # Errors
    ///
    /// Returns an error if the bounded current-pointer transaction fails or
    /// its generation identity/sequence cannot be decoded.
    pub async fn current_generation_record(
        &self,
        project_id: &ProjectId,
    ) -> Result<Option<CurrentGenerationRecord>, StorageError> {
        self.current_generation_record_bounded(project_id, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve the current generation pointer under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline is invalid, the read-only transaction
    /// fails, or the current generation row is malformed.
    pub async fn current_generation_record_bounded(
        &self,
        project_id: &ProjectId,
        statement_timeout: Duration,
    ) -> Result<Option<CurrentGenerationRecord>, StorageError> {
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"SELECT generations.generation_id::text, generations.generation_sequence
                FROM {schema}."projects" AS projects
                INNER JOIN {schema}."index_generations" AS generations
                    ON generations.project_id = projects.project_id
                   AND generations.generation_id = projects.current_generation_id
                   AND generations.state = 'current'
                WHERE projects.project_id = CAST($1 AS uuid)"#
        );
        let row = query(AssertSqlSafe(sql))
            .bind(project_id.as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| database_error("current-generation-read"))?;
        commit_bounded_read(transaction, "current-generation-read-commit").await?;
        row.as_ref().map(decode_generation).transpose()
    }

    /// Resolve one exact canonical path in the current generation.
    /// # Errors
    ///
    /// Returns an error if the expected generation is not current or the exact
    /// path record cannot be queried or decoded.
    pub async fn exact_current_file_by_path(
        &self,
        request: CurrentFileLookup<'_>,
    ) -> Result<Option<CurrentFileRecord>, StorageError> {
        self.exact_current_file_by_path_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve one exact current path under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline is invalid, the generation fence
    /// fails, or the exact file row is malformed.
    pub async fn exact_current_file_by_path_bounded(
        &self,
        request: CurrentFileLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Option<CurrentFileRecord>, StorageError> {
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(request.project_id, request.expected_generation_id),
        )
        .await?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"SELECT files.generation_id::text, files.file_id::text,
                       files.normalized_path, files.language, files.content_hash
                FROM {schema}."files" AS files
                WHERE files.project_id = CAST($1 AS uuid)
                  AND files.normalized_path = $2
                  AND files.generation_id = CAST($3 AS uuid)
                ORDER BY files.file_id
                LIMIT 1"#
        );
        let row = query(AssertSqlSafe(sql))
            .bind(request.project_id.as_str())
            .bind(request.path.as_str())
            .bind(request.expected_generation_id.as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| database_error("exact-path-lookup"))?;
        commit_bounded_read(transaction, "exact-path-lookup-commit").await?;
        row.as_ref().map(decode_file).transpose()
    }

    /// List a bounded, path-ordered file inventory from the current generation.
    /// # Errors
    ///
    /// Returns an error if inventory bounds are invalid, the expected
    /// generation is not current, or a filtered file row cannot be decoded.
    pub async fn current_files(
        &self,
        request: CurrentFilesLookup<'_>,
    ) -> Result<Vec<CurrentFileRecord>, StorageError> {
        self.current_files_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// List current files under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the explicit deadline or file limit is invalid, the
    /// generation fence fails, or PostgreSQL returns a malformed file row.
    pub async fn current_files_bounded(
        &self,
        request: CurrentFilesLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Vec<CurrentFileRecord>, StorageError> {
        validate_limit(request.limit, MAX_FILE_LIST_LIMIT)?;
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(request.project_id, request.expected_generation_id),
        )
        .await?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"SELECT files.generation_id::text, files.file_id::text,
                       files.normalized_path, files.language, files.content_hash
                FROM {schema}."files" AS files
                WHERE files.project_id = CAST($1 AS uuid)
                  AND files.generation_id = CAST($5 AS uuid)
                  AND (
                      CAST($2 AS text) IS NULL
                      OR files.normalized_path = CAST($2 AS text)
                      OR files.normalized_path LIKE CAST($2 AS text) || '/%'
                  )
                  AND (
                      CAST($3 AS text) IS NULL
                      OR files.language = CAST($3 AS text)
                  )
                ORDER BY files.normalized_path, files.file_id
                LIMIT $4"#
        );
        let directory = request.directory.map(NormalizedPath::as_str);
        let language = request.language.map(SourceLanguage::as_str);
        let rows = query(AssertSqlSafe(sql))
            .bind(request.project_id.as_str())
            .bind(directory)
            .bind(language)
            .bind(i64::from(request.limit))
            .bind(request.expected_generation_id.as_str())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("current-files-read"))?;
        commit_bounded_read(transaction, "current-files-read-commit").await?;
        rows.iter().map(decode_file).collect()
    }

    /// Resolve an exact qualified declaration name in the current generation.
    /// # Errors
    ///
    /// Returns an error if exact text/limit validation fails, the generation
    /// is no longer current, or a matching symbol record is malformed.
    pub async fn exact_current_symbols_by_name(
        &self,
        request: ExactTextLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.exact_current_symbols_by_name_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve an exact declaration name under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline, exact text, or limit is invalid, the
    /// generation fence fails, or a symbol row cannot be decoded.
    pub async fn exact_current_symbols_by_name_bounded(
        &self,
        request: ExactTextLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = exact_symbol_sql(&schema, request.text.trim());
        let rows = self
            .fetch_exact_rows(ExactRowsQuery {
                request,
                statement_timeout,
                sql,
                validation_field: "name",
                operation: "exact-name-lookup",
                commit_operation: "exact-name-lookup-commit",
            })
            .await?;
        rows.iter().map(decode_symbol).collect()
    }

    /// Resolve symbols owned by one exact current-generation file.
    /// # Errors
    ///
    /// Returns an error if the symbol limit is invalid, the generation fence
    /// fails, or the file's symbol records cannot be queried or decoded.
    pub async fn current_symbols_by_file(
        &self,
        request: CurrentFileSymbolsLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.current_symbols_by_file_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve one file's symbols under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline or symbol limit is invalid, the
    /// generation is not current, or a file-owned symbol row is malformed.
    pub async fn current_symbols_by_file_bounded(
        &self,
        request: CurrentFileSymbolsLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        validate_limit(request.limit, MAX_LOOKUP_LIMIT)?;
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(request.project_id, request.expected_generation_id),
        )
        .await?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = symbol_select(&schema, "symbols.file_id = CAST($2 AS uuid)");
        let rows = query(AssertSqlSafe(sql))
            .bind(request.project_id.as_str())
            .bind(request.file_id.as_str())
            .bind(i64::from(request.limit))
            .bind(request.expected_generation_id.as_str())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("symbols-by-file"))?;
        commit_bounded_read(transaction, "symbols-by-file-commit").await?;
        rows.iter().map(decode_symbol).collect()
    }

    /// Resolve symbols whose indexed ranges overlap one exact source range.
    /// # Errors
    ///
    /// Returns an error if the source range/result bound is invalid, the
    /// generation fence fails, or an overlapping symbol cannot be decoded.
    pub async fn current_symbols_at_range(
        &self,
        request: CurrentSourceRangeLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.current_symbols_at_range_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve overlapping symbols under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline, line range, or result limit is
    /// invalid, the generation is stale, or an overlapping row is malformed.
    pub async fn current_symbols_at_range_bounded(
        &self,
        request: CurrentSourceRangeLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        validate_source_range(request.start_line, request.end_line)?;
        validate_limit(request.limit, MAX_LOOKUP_LIMIT)?;
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(request.project_id, request.expected_generation_id),
        )
        .await?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"SELECT symbols.generation_id::text, symbols.symbol_id::text,
                       symbols.file_id::text, files.normalized_path, files.language,
                       symbols.symbol_kind, symbols.qualified_name, symbols.signature,
                       symbols.start_line, symbols.end_line,
                       symbols.visibility, symbols.exported, symbols.default_export,
                       symbols.async_symbol, symbols.static_member, symbols.declaration_only
                FROM {schema}."symbols" AS symbols
                INNER JOIN {schema}."files" AS files
                    ON files.project_id = symbols.project_id
                   AND files.generation_id = symbols.generation_id
                   AND files.file_id = symbols.file_id
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.generation_id = CAST($6 AS uuid)
                  AND files.normalized_path = $2
                  AND symbols.end_line >= $3
                  AND symbols.start_line <= $4
                ORDER BY
                    (symbols.end_line - symbols.start_line),
                    symbols.start_line,
                    symbols.end_line,
                    symbols.symbol_id
                LIMIT $5"#
        );
        let rows = query(AssertSqlSafe(sql))
            .bind(request.project_id.as_str())
            .bind(request.path.as_str())
            .bind(i64::from(request.start_line))
            .bind(i64::from(request.end_line))
            .bind(i64::from(request.limit))
            .bind(request.expected_generation_id.as_str())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("symbols-at-range"))?;
        commit_bounded_read(transaction, "symbols-at-range-commit").await?;
        rows.iter().map(decode_symbol).collect()
    }

    /// Hydrate a bounded set of symbol identities from only the current generation.
    /// # Errors
    ///
    /// Returns an error if the symbol set exceeds its bound, the expected
    /// generation is not current, or a hydrated symbol row is malformed.
    pub async fn current_symbols_by_ids(
        &self,
        request: CurrentSymbolSetLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.current_symbols_by_ids_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Hydrate a bounded symbol set under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline or symbol-set bound is invalid, the
    /// generation fence fails, or an identified symbol cannot be decoded.
    pub async fn current_symbols_by_ids_bounded(
        &self,
        request: CurrentSymbolSetLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        validate_symbol_set(request.symbol_ids)?;
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(request.project_id, request.expected_generation_id),
        )
        .await?;
        if request.symbol_ids.is_empty() {
            commit_bounded_read(transaction, "symbols-by-id-empty-commit").await?;
            return Ok(Vec::new());
        }
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = symbol_select(&schema, "symbols.symbol_id = ANY(CAST($2 AS uuid[]))");
        let ids = request
            .symbol_ids
            .iter()
            .map(|symbol_id| symbol_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let rows = query(AssertSqlSafe(sql))
            .bind(request.project_id.as_str())
            .bind(ids)
            .bind(i64::try_from(request.symbol_ids.len()).map_err(|_| invalid("symbol_ids"))?)
            .bind(request.expected_generation_id.as_str())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("symbols-by-id"))?;
        commit_bounded_read(transaction, "symbols-by-id-commit").await?;
        rows.iter().map(decode_symbol).collect()
    }

    /// Discover one bounded top-of-stack category from typed current-generation facts.
    /// # Errors
    ///
    /// Returns an error if the category limit is invalid, the generation fence
    /// fails, or typed entry-point evidence cannot be queried or decoded.
    pub async fn current_entry_points(
        &self,
        request: CurrentEntryPointsLookup<'_>,
    ) -> Result<CurrentEntryPointPage, StorageError> {
        self.current_entry_points_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Discover one entry-point category under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline or category limit is invalid, the
    /// generation is stale, or an entry-point record violates its contract.
    pub async fn current_entry_points_bounded(
        &self,
        request: CurrentEntryPointsLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<CurrentEntryPointPage, StorageError> {
        validate_limit(request.limit, MAX_ENTRY_POINT_LIMIT)?;
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(request.project_id, request.expected_generation_id),
        )
        .await?;
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = entry_point_sql(&schema, request.bucket);
        let rows = query(AssertSqlSafe(sql))
            .bind(request.project_id.as_str())
            .bind(i64::from(request.limit))
            .bind(request.expected_generation_id.as_str())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("entry-points"))?;
        commit_bounded_read(transaction, "entry-points-commit").await?;
        let total = match rows.first() {
            Some(row) => u64::try_from(read_i64(
                row,
                SYMBOL_DECLARATION_ONLY_COLUMN + 1,
                "entry_point_total",
            )?)
            .map_err(|_| corrupt("entry_point_total"))?,
            None => 0,
        };
        let symbols = rows
            .iter()
            .map(decode_symbol)
            .collect::<Result<Vec<_>, _>>()?;
        let returned = u64::try_from(symbols.len()).map_err(|_| corrupt("entry_point_returned"))?;
        Ok(CurrentEntryPointPage {
            bucket: request.bucket,
            total,
            truncated: total > returned,
            symbols,
        })
    }

    /// Resolve exact reference text, including unresolved evidence, in the current generation.
    /// # Errors
    ///
    /// Returns an error if exact text/limit validation fails, the generation
    /// fence fails, or matching reference evidence cannot be decoded.
    pub async fn exact_current_references_by_name(
        &self,
        request: ExactTextLookup<'_>,
    ) -> Result<Vec<CurrentReferenceRecord>, StorageError> {
        self.exact_current_references_by_name_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve exact reference evidence under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline, exact text, or result limit is
    /// invalid, the generation is stale, or a reference row is malformed.
    pub async fn exact_current_references_by_name_bounded(
        &self,
        request: ExactTextLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Vec<CurrentReferenceRecord>, StorageError> {
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = exact_reference_sql(&schema);
        let rows = self
            .fetch_exact_rows(ExactRowsQuery {
                request,
                statement_timeout,
                sql,
                validation_field: "reference_name",
                operation: "exact-reference-lookup",
                commit_operation: "exact-reference-lookup-commit",
            })
            .await?;
        rows.iter().map(decode_reference).collect()
    }

    /// Read a bounded incoming or outgoing structural frontier from only the
    /// current generation. Callers own breadth/depth policy.
    /// # Errors
    ///
    /// Returns an error if the frontier/result bounds are invalid, the
    /// generation fence fails, or a structural edge cannot be decoded.
    pub async fn current_graph_edges(
        &self,
        request: CurrentGraphLookup<'_>,
    ) -> Result<Vec<CurrentGraphEdge>, StorageError> {
        self.current_graph_edges_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Read a bounded graph frontier under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline, frontier, or edge limit is invalid,
    /// the generation is stale, or a graph row is malformed.
    pub async fn current_graph_edges_bounded(
        &self,
        request: CurrentGraphLookup<'_>,
        statement_timeout: Duration,
    ) -> Result<Vec<CurrentGraphEdge>, StorageError> {
        validate_symbol_set(request.frontier)?;
        validate_limit(request.limit, MAX_EDGE_LIMIT)?;
        let mut transaction = begin_bounded_read(self, statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(request.project_id, request.expected_generation_id),
        )
        .await?;
        if request.frontier.is_empty() {
            commit_bounded_read(transaction, "graph-frontier-empty-commit").await?;
            return Ok(Vec::new());
        }
        let (predicate, origin, adjacent) = match request.direction {
            GraphDirection::Outgoing => (
                "edges.source_symbol_id = ANY(CAST($2 AS uuid[]))",
                "edges.source_symbol_id",
                "edges.target_symbol_id",
            ),
            GraphDirection::Incoming => (
                "edges.target_symbol_id = ANY(CAST($2 AS uuid[]))",
                "edges.target_symbol_id",
                "edges.source_symbol_id",
            ),
        };
        let schema = crate::database::quoted_schema(&self.schema);
        let sql = format!(
            r#"WITH ranked_edges AS MATERIALIZED (
                    SELECT edges.generation_id, edges.source_symbol_id,
                           edges.target_symbol_id, edges.edge_kind,
                           edges.confidence, edges.provenance, edges.site_count,
                           row_number() OVER (
                               PARTITION BY {origin}
                               ORDER BY edges.confidence DESC, edges.site_count DESC,
                                        edges.edge_kind, edges.provenance,
                                        edges.source_symbol_id, edges.target_symbol_id
                           ) AS frontier_rank
                    FROM {schema}."edges" AS edges
                    WHERE edges.project_id = CAST($1 AS uuid)
                      AND edges.generation_id = CAST($4 AS uuid)
                      AND {predicate}
                      AND (
                          $5
                          OR NOT EXISTS (
                              SELECT 1
                              FROM {schema}."symbols" AS adjacent_symbols
                              JOIN {schema}."files" AS adjacent_files
                                ON adjacent_files.project_id = adjacent_symbols.project_id
                               AND adjacent_files.generation_id = adjacent_symbols.generation_id
                               AND adjacent_files.file_id = adjacent_symbols.file_id
                              WHERE adjacent_symbols.project_id = edges.project_id
                                AND adjacent_symbols.generation_id = edges.generation_id
                                AND adjacent_symbols.symbol_id = {adjacent}
                                AND (
                                    adjacent_files.normalized_path ~* '(^|/)(__tests__|tests?|specs?)(/|$)'
                                    OR adjacent_files.normalized_path ~* '(\.test\.|\.spec\.|_test\.|_spec\.)'
                                )
                          )
                      )
                )
                SELECT generation_id::text, source_symbol_id::text,
                       target_symbol_id::text, edge_kind, confidence,
                       provenance, site_count
                FROM ranked_edges
                ORDER BY frontier_rank, confidence DESC, site_count DESC,
                         edge_kind, provenance, source_symbol_id, target_symbol_id
                LIMIT $3"#
        );
        let ids = request
            .frontier
            .iter()
            .map(|symbol_id| symbol_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let rows = query(AssertSqlSafe(sql))
            .bind(request.project_id.as_str())
            .bind(ids)
            .bind(i64::from(request.limit))
            .bind(request.expected_generation_id.as_str())
            .bind(request.include_test_targets)
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("graph-frontier-read"))?;
        commit_bounded_read(transaction, "graph-frontier-read-commit").await?;
        rows.iter().map(decode_edge).collect()
    }

    async fn fetch_exact_rows(
        &self,
        input: ExactRowsQuery<'_>,
    ) -> Result<Vec<sqlx_postgres::PgRow>, StorageError> {
        let text = validate_exact_text(input.request.text, input.validation_field)?;
        validate_limit(input.request.limit, MAX_LOOKUP_LIMIT)?;
        let mut transaction = begin_bounded_read(self, input.statement_timeout).await?;
        require_expected_current_generation(
            &mut transaction,
            &self.schema,
            CurrentGenerationLookup::new(
                input.request.project_id,
                input.request.expected_generation_id,
            ),
        )
        .await?;
        let rows = query(AssertSqlSafe(input.sql))
            .bind(input.request.project_id.as_str())
            .bind(text)
            .bind(i64::from(input.request.limit))
            .bind(input.request.expected_generation_id.as_str())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error(input.operation))?;
        commit_bounded_read(transaction, input.commit_operation).await?;
        Ok(rows)
    }
}

pub(crate) async fn begin_bounded_read(
    database: &CartographDatabase,
    statement_timeout: Duration,
) -> Result<sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>, StorageError> {
    if statement_timeout.is_zero() {
        return Err(invalid("statement_timeout"));
    }
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("bounded-read-begin"))?;
    if query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await
        .is_err()
    {
        let _ = transaction.rollback().await;
        return Err(database_error("bounded-read-isolation"));
    }
    if crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
        .await
        .is_err()
    {
        let _ = transaction.rollback().await;
        return Err(invalid("statement_timeout"));
    }
    Ok(transaction)
}

pub(crate) async fn require_expected_current_generation(
    connection: &mut sqlx_postgres::PgConnection,
    schema: &cartograph_config::DatabaseSchema,
    generation: CurrentGenerationLookup<'_>,
) -> Result<(), StorageError> {
    let sql = format!(
        r#"SELECT current_generation_id = CAST($2 AS uuid) AS matches
            FROM {}."projects"
            WHERE project_id = CAST($1 AS uuid)"#,
        crate::database::quoted_schema(schema)
    );
    let matches = query(AssertSqlSafe(sql))
        .bind(generation.project_id.as_str())
        .bind(generation.expected_generation_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("expected-generation-read"))?
        .and_then(|row| row.try_get::<Option<bool>, _>("matches").ok().flatten())
        .unwrap_or(false);
    matches.ok_or(StorageError::CurrentGenerationChanged)
}

pub(crate) async fn commit_bounded_read(
    transaction: sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    operation: &'static str,
) -> Result<(), StorageError> {
    transaction
        .commit()
        .await
        .map_err(|_| database_error(operation))
}

fn exact_reference_sql(schema: &str) -> String {
    format!(
        r#"SELECT refs.reference_id, refs.generation_id::text,
                   refs.file_id::text, files.normalized_path,
                   refs.owner_symbol_id::text,
                   refs.target_symbol_id::text,
                   refs.reference_name, refs.reference_kind,
                   refs.start_byte, refs.end_byte,
                   refs.confidence, refs.resolution_provenance, refs.site_count,
                   refs.span_precision
                FROM {schema}."references" AS refs
                INNER JOIN {schema}."files" AS files
                    ON files.project_id = refs.project_id
                   AND files.generation_id = refs.generation_id
                   AND files.file_id = refs.file_id
                WHERE refs.project_id = CAST($1 AS uuid)
                  AND refs.reference_name = $2
                  AND refs.generation_id = CAST($4 AS uuid)
                ORDER BY refs.file_id, refs.start_byte, refs.reference_id
                LIMIT $3"#
    )
}

fn exact_symbol_sql(schema: &str, requested_name: &str) -> String {
    let predicate = if requested_name.contains("::") || requested_name.contains('.') {
        "symbols.qualified_name = $2"
    } else {
        "symbols.simple_name = $2"
    };
    format!(
        r#"SELECT symbols.generation_id::text, symbols.symbol_id::text,
                   symbols.file_id::text, files.normalized_path, files.language,
                   symbols.symbol_kind, symbols.qualified_name, symbols.signature,
                   symbols.start_line, symbols.end_line,
                   symbols.visibility, symbols.exported, symbols.default_export,
                   symbols.async_symbol, symbols.static_member, symbols.declaration_only
            FROM {schema}."symbols" AS symbols
            INNER JOIN {schema}."files" AS files
                ON files.project_id = symbols.project_id
               AND files.generation_id = symbols.generation_id
               AND files.file_id = symbols.file_id
            WHERE symbols.project_id = CAST($1 AS uuid)
              AND symbols.generation_id = CAST($4 AS uuid)
              AND {predicate}
            ORDER BY files.normalized_path, symbols.start_line, symbols.symbol_id
            LIMIT $3"#
    )
}

fn symbol_select(schema: &str, predicate: &str) -> String {
    format!(
        r#"SELECT symbols.generation_id::text, symbols.symbol_id::text,
                   symbols.file_id::text, files.normalized_path, files.language,
                   symbols.symbol_kind, symbols.qualified_name, symbols.signature,
                   symbols.start_line, symbols.end_line,
                   symbols.visibility, symbols.exported, symbols.default_export,
                   symbols.async_symbol, symbols.static_member, symbols.declaration_only
            FROM {schema}."symbols" AS symbols
            INNER JOIN {schema}."files" AS files
                ON files.project_id = symbols.project_id
               AND files.generation_id = symbols.generation_id
               AND files.file_id = symbols.file_id
            WHERE symbols.project_id = CAST($1 AS uuid)
              AND symbols.generation_id = CAST($4 AS uuid)
              AND {predicate}
            ORDER BY files.normalized_path, symbols.start_line, symbols.symbol_id
            LIMIT $3"#
    )
}

fn entry_point_sql(schema: &str, bucket: EntryPointBucket) -> String {
    let production_path = r"NOT (
        files.normalized_path ~* '(^|/)(__tests__|__mocks__|tests?|specs?|integration|testing|testlib|fixtures?|test-beds?)(/|$)'
        OR files.normalized_path ~* '(\.test\.|\.spec\.|_test\.|_spec\.)[a-z0-9]+$'
        OR files.normalized_path ~ '([A-Za-z](Test|Tests|TestCase|Spec))\.[a-z0-9]+$'
        OR files.normalized_path ~* '(^|/)(test[-_][^/]*|test|mocks?|fixtures?)\.[a-z0-9]+$'
    )";
    let cli_path = r"files.normalized_path ~* '(^|/)(bin|cli|commands?)/'";
    let exportable = r"symbols.symbol_kind IN (
        'function', 'method', 'class', 'struct', 'interface', 'trait', 'enum',
        'type_alias', 'constant', 'variable', 'module', 'namespace', 'component', 'resource'
    )";
    let predicate = match bucket {
        EntryPointBucket::Routes => format!(
            "symbols.symbol_kind = 'route' AND lower(symbols.simple_name) NOT LIKE 'cmd %' AND {production_path}"
        ),
        EntryPointBucket::CliCommands => format!(
            "symbols.symbol_kind = 'route' AND lower(symbols.simple_name) LIKE 'cmd %' AND {production_path}"
        ),
        EntryPointBucket::McpTools => format!(
            r#"symbols.exported
                AND symbols.symbol_kind = 'constant'
                AND symbols.simple_name ~ '^[A-Z][A-Z0-9_]*_TOOL$'
                AND {production_path}
                AND EXISTS (
                    SELECT 1
                    FROM {schema}."search_documents" AS documents
                    WHERE documents.project_id = symbols.project_id
                      AND documents.generation_id = symbols.generation_id
                      AND documents.symbol_id = symbols.symbol_id
                      AND position('cartograph_' IN lower(documents.code)) > 0
                )"#
        ),
        EntryPointBucket::CliFiles => {
            format!("symbols.exported AND {exportable} AND {production_path} AND {cli_path}")
        }
        EntryPointBucket::PublicExports => format!(
            r#"symbols.exported
                AND {exportable}
                AND {production_path}
                AND NOT ({cli_path})
                AND NOT EXISTS (
                    SELECT 1
                    FROM {schema}."edges" AS incoming
                    WHERE incoming.project_id = symbols.project_id
                      AND incoming.generation_id = symbols.generation_id
                      AND incoming.target_symbol_id = symbols.symbol_id
                      AND incoming.edge_kind IN (
                          'calls', 'references', 'instantiates', 'type_of',
                          'returns', 'extends', 'implements'
                      )
                )"#
        ),
    };
    format!(
        r#"SELECT symbols.generation_id::text, symbols.symbol_id::text,
                   symbols.file_id::text, files.normalized_path, files.language,
                   symbols.symbol_kind, symbols.qualified_name, symbols.signature,
                   symbols.start_line, symbols.end_line,
                   symbols.visibility, symbols.exported, symbols.default_export,
                   symbols.async_symbol, symbols.static_member, symbols.declaration_only,
                   count(*) OVER ()::bigint
            FROM {schema}."symbols" AS symbols
            INNER JOIN {schema}."files" AS files
                ON files.project_id = symbols.project_id
               AND files.generation_id = symbols.generation_id
               AND files.file_id = symbols.file_id
            WHERE symbols.project_id = CAST($1 AS uuid)
              AND symbols.generation_id = CAST($3 AS uuid)
              AND {predicate}
            ORDER BY files.normalized_path, symbols.start_line, symbols.symbol_id
            LIMIT $2"#
    )
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

const fn validate_source_range(start_line: u32, end_line: u32) -> Result<(), StorageError> {
    if start_line == 0 || end_line < start_line {
        return Err(invalid("source_range"));
    }
    Ok(())
}

fn validate_symbol_set(symbol_ids: &[SymbolId]) -> Result<(), StorageError> {
    if symbol_ids.len() > MAX_FRONTIER_SYMBOLS {
        return Err(invalid("symbol_ids"));
    }
    Ok(())
}

fn decode_generation(row: &sqlx_postgres::PgRow) -> Result<CurrentGenerationRecord, StorageError> {
    let sequence = read_i64(row, 1, "generation_sequence")?;
    Ok(CurrentGenerationRecord {
        generation_id: parse_stored_generation_id(row, 0)?,
        sequence: u64::try_from(sequence).map_err(|_| corrupt("generation_sequence"))?,
    })
}

fn decode_file(row: &sqlx_postgres::PgRow) -> Result<CurrentFileRecord, StorageError> {
    Ok(CurrentFileRecord {
        generation_id: parse_stored_generation_id(row, 0)?,
        file_id: parse_file_id(row, 1)?,
        path: parse_path(row, 2)?,
        language: read_stored_string(row, 3, "language")?,
        content_hash: ContentDigest::parse(&read_stored_string(row, 4, "content_hash")?)
            .map_err(|_| corrupt("content_hash"))?,
    })
}

pub(crate) fn decode_symbol(
    row: &sqlx_postgres::PgRow,
) -> Result<CurrentSymbolRecord, StorageError> {
    Ok(CurrentSymbolRecord {
        generation_id: parse_stored_generation_id(row, 0)?,
        symbol_id: parse_symbol_id(row, 1)?,
        file_id: parse_file_id(row, 2)?,
        path: parse_path(row, SYMBOL_PATH_COLUMN)?,
        language: read_stored_string(row, SYMBOL_LANGUAGE_COLUMN, "language")?,
        symbol_kind: read_stored_string(row, SYMBOL_KIND_COLUMN, "symbol_kind")?,
        qualified_name: read_stored_string(row, SYMBOL_QUALIFIED_NAME_COLUMN, "qualified_name")?,
        signature: read_stored_string(row, SYMBOL_SIGNATURE_COLUMN, "signature")?,
        start_line: read_u32(row, SYMBOL_START_LINE_COLUMN, "start_line")?,
        end_line: read_u32(row, SYMBOL_END_LINE_COLUMN, "end_line")?,
        visibility: parse_optional_visibility(row, SYMBOL_VISIBILITY_COLUMN)?,
        export: decode_symbol_export(row)?,
        execution: decode_symbol_execution(row)?,
        declaration_only: read_bool(row, SYMBOL_DECLARATION_ONLY_COLUMN, "declaration_only")?,
    })
}

/// Named/default export state stored on one current symbol row.
fn decode_symbol_export(row: &sqlx_postgres::PgRow) -> Result<SymbolExportFlags, StorageError> {
    Ok(SymbolExportFlags::new(
        read_bool(row, SYMBOL_EXPORTED_COLUMN, "exported")?,
        read_bool(row, SYMBOL_DEFAULT_EXPORT_COLUMN, "default_export")?,
    ))
}

/// Async/static execution modifiers stored on one current symbol row.
fn decode_symbol_execution(
    row: &sqlx_postgres::PgRow,
) -> Result<SymbolExecutionFlags, StorageError> {
    Ok(SymbolExecutionFlags {
        async_symbol: read_bool(row, SYMBOL_ASYNC_COLUMN, "async_symbol")?,
        static_member: read_bool(row, SYMBOL_STATIC_COLUMN, "static_member")?,
    })
}

fn decode_reference(row: &sqlx_postgres::PgRow) -> Result<CurrentReferenceRecord, StorageError> {
    let reference_id = read_i64(row, 0, "reference_id")?;
    let start_byte = read_i64(row, REFERENCE_START_BYTE_COLUMN, "start_byte")?;
    let end_byte = read_i64(row, REFERENCE_END_BYTE_COLUMN, "end_byte")?;
    let site_count = read_i64(row, REFERENCE_SITE_COUNT_COLUMN, "site_count")?;
    Ok(CurrentReferenceRecord {
        reference_id: u64::try_from(reference_id).map_err(|_| corrupt("reference_id"))?,
        generation_id: parse_stored_generation_id(row, 1)?,
        file_id: parse_file_id(row, 2)?,
        path: parse_path(row, REFERENCE_PATH_COLUMN)?,
        owner_symbol_id: parse_optional_symbol_id(row, REFERENCE_OWNER_COLUMN, "owner_symbol_id")?,
        target_symbol_id: parse_optional_symbol_id(
            row,
            REFERENCE_TARGET_COLUMN,
            "target_symbol_id",
        )?,
        reference_name: read_stored_string(row, REFERENCE_NAME_COLUMN, "reference_name")?,
        reference_kind: read_stored_string(row, REFERENCE_KIND_COLUMN, "reference_kind")?,
        start_byte: u64::try_from(start_byte).map_err(|_| corrupt("start_byte"))?,
        end_byte: u64::try_from(end_byte).map_err(|_| corrupt("end_byte"))?,
        confidence: read_confidence(row, REFERENCE_CONFIDENCE_COLUMN)?,
        provenance: read_stored_string(row, REFERENCE_PROVENANCE_COLUMN, "resolution_provenance")?,
        site_count: u32::try_from(site_count).map_err(|_| corrupt("site_count"))?,
        span_precision: ReferenceSpanPrecision::parse(&read_stored_string(
            row,
            REFERENCE_SPAN_PRECISION_COLUMN,
            "span_precision",
        )?)
        .map_err(|()| corrupt("span_precision"))?,
    })
}

fn decode_edge(row: &sqlx_postgres::PgRow) -> Result<CurrentGraphEdge, StorageError> {
    let site_count = read_i64(row, EDGE_SITE_COUNT_COLUMN, "site_count")?;
    Ok(CurrentGraphEdge {
        generation_id: parse_stored_generation_id(row, 0)?,
        source_symbol_id: parse_symbol_id(row, 1)?,
        target_symbol_id: parse_symbol_id(row, 2)?,
        edge_kind: read_stored_string(row, EDGE_KIND_COLUMN, "edge_kind")?,
        confidence: read_confidence(row, EDGE_CONFIDENCE_COLUMN)?,
        provenance: read_stored_string(row, EDGE_PROVENANCE_COLUMN, "provenance")?,
        site_count: u32::try_from(site_count).map_err(|_| corrupt("site_count"))?,
    })
}

fn parse_file_id(row: &sqlx_postgres::PgRow, index: usize) -> Result<FileId, StorageError> {
    let raw = read_stored_string(row, index, "file_id")?;
    FileId::parse(&raw).map_err(|_| corrupt("file_id"))
}

fn parse_symbol_id(row: &sqlx_postgres::PgRow, index: usize) -> Result<SymbolId, StorageError> {
    let raw = read_stored_string(row, index, "symbol_id")?;
    SymbolId::parse(&raw).map_err(|_| corrupt("symbol_id"))
}

fn parse_optional_symbol_id(
    row: &sqlx_postgres::PgRow,
    index: usize,
    field: &'static str,
) -> Result<Option<SymbolId>, StorageError> {
    let raw = row
        .try_get::<Option<String>, _>(index)
        .map_err(|_| corrupt(field))?;
    raw.map(|value| SymbolId::parse(&value).map_err(|_| corrupt(field)))
        .transpose()
}

fn parse_optional_visibility(
    row: &sqlx_postgres::PgRow,
    index: usize,
) -> Result<Option<Visibility>, StorageError> {
    let raw = row
        .try_get::<Option<String>, _>(index)
        .map_err(|_| corrupt("visibility"))?;
    raw.map(|value| Visibility::from_stable_str(&value).ok_or_else(|| corrupt("visibility")))
        .transpose()
}

fn parse_path(row: &sqlx_postgres::PgRow, index: usize) -> Result<NormalizedPath, StorageError> {
    let raw = read_stored_string(row, index, "normalized_path")?;
    NormalizedPath::parse(&raw).map_err(|_| corrupt("normalized_path"))
}

fn read_i64(
    row: &sqlx_postgres::PgRow,
    index: usize,
    field: &'static str,
) -> Result<i64, StorageError> {
    row.try_get::<i64, _>(index).map_err(|_| corrupt(field))
}

fn read_u32(
    row: &sqlx_postgres::PgRow,
    index: usize,
    field: &'static str,
) -> Result<u32, StorageError> {
    let value = row.try_get::<i32, _>(index).map_err(|_| corrupt(field))?;
    u32::try_from(value).map_err(|_| corrupt(field))
}

fn read_confidence(row: &sqlx_postgres::PgRow, index: usize) -> Result<f32, StorageError> {
    let value = row
        .try_get::<f32, _>(index)
        .map_err(|_| corrupt("confidence"))?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(corrupt("confidence"));
    }
    Ok(value)
}

const fn invalid(field: &'static str) -> StorageError {
    StorageError::InvalidInput { field }
}

const fn corrupt(field: &'static str) -> StorageError {
    stored_value_error(field)
}

const fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
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

    #[test]
    fn exact_reference_sql_uses_a_non_keyword_alias() {
        let sql = exact_reference_sql("\"fixture\"");
        assert!(sql.contains("AS refs"));
        assert!(!sql.contains("AS references"));
        assert!(sql.contains("refs.target_symbol_id::text"));
        assert!(sql.contains("ORDER BY refs.file_id, refs.start_byte, refs.reference_id"));
    }

    #[test]
    fn exact_symbol_sql_uses_only_sargable_full_or_simple_name_predicates() {
        let simple = exact_symbol_sql("\"fixture\"", "tagscanary");
        assert!(simple.contains("symbols.simple_name = $2"));
        assert!(!simple.contains("right("));
        assert!(!simple.contains("qualified_name = $2"));

        for qualified in ["T2Elixir.tagscanary", "module::tagscanary"] {
            let full = exact_symbol_sql("\"fixture\"", qualified);
            assert!(full.contains("symbols.qualified_name = $2"), "{qualified}");
            assert!(!full.contains("simple_name = $2"), "{qualified}");
            assert!(!full.contains("right("), "{qualified}");
        }
    }

    #[test]
    fn entry_point_queries_use_typed_semantics_and_generation_scoped_edges() {
        let schema = r#""cartograph_test""#;
        let routes = entry_point_sql(schema, EntryPointBucket::Routes);
        let commands = entry_point_sql(schema, EntryPointBucket::CliCommands);
        let tools = entry_point_sql(schema, EntryPointBucket::McpTools);
        let cli = entry_point_sql(schema, EntryPointBucket::CliFiles);
        let public = entry_point_sql(schema, EntryPointBucket::PublicExports);

        for sql in [&routes, &commands, &tools, &cli, &public] {
            assert!(sql.contains("symbols.generation_id = CAST($3 AS uuid)"));
            assert!(sql.contains("count(*) OVER ()::bigint"));
            assert!(sql.contains("test-beds?"));
        }
        assert!(routes.contains("symbols.symbol_kind = 'route'"));
        assert!(commands.contains("LIKE 'cmd %'"));
        assert!(tools.contains("position('cartograph_' IN lower(documents.code)) > 0"));
        assert!(cli.contains("(bin|cli|commands?)/"));
        for sql in [&tools, &cli, &public] {
            assert!(sql.contains("symbols.exported"));
        }
        assert!(public.contains("incoming.target_symbol_id = symbols.symbol_id"));
        assert!(public.contains("'calls', 'references', 'instantiates', 'type_of'"));
        assert!(public.contains("'constant'"));

        assert_eq!(
            EntryPointBucket::from_stable_str("cli"),
            Some(EntryPointBucket::CliCommands)
        );
        assert_eq!(EntryPointBucket::from_stable_str("unknown"), None);
    }
}
