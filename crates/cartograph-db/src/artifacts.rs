use std::{collections::BTreeMap, time::Duration};

use cartograph_domain::{
    ContentDigest, GenerationId, ModelId, NormalizedPath, ProjectId, SymbolId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx_core::{column::ColumnIndex, query::query, row::Row, sql_str::AssertSqlSafe};

use crate::{
    CartographDatabase, StorageError,
    database::{
        ProjectReadRequest, quoted_schema, read_project_rows, set_local_statement_timeout,
        validate_bounded_limit as validate_limit, validate_json_object,
    },
};

const ARTIFACT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ARTIFACT_BODY_BYTES: usize = 65_536;
const MAX_ARTIFACT_KEY_BYTES: usize = 4_096;
const MAX_ARTIFACT_METADATA_BYTES: usize = 65_536;
const MAX_ARTIFACT_LIMIT: u16 = 500;
const MAX_INLINE_FILE_SUMMARIES: usize = 80;
const MAX_SUMMARY_BATCH: u16 = 40;
const MAX_STRUCTURAL_SUMMARY_BATCH: u16 = 320;
const MAX_NEIGHBOR_SUMMARY_BATCH: u16 = 320;
const MAX_NEIGHBOR_SUMMARY_SOURCES: usize = 2_560;
const MAX_FILE_SUMMARY_BATCH: u16 = 200;
const MAX_SUMMARY_ROLLUP_ITEMS: u16 = 50;
const MAX_MODULE_SUMMARY_ROLLUP_ITEMS: u16 = 30;
const MIN_MODULE_SUMMARY_SYMBOLS: u64 = 3;
const MAX_ROLE_BATCH: u16 = 320;
const SUMMARY_SOURCE_MAXIMUM_CHARACTERS: i32 = 2_048;
const SUMMARY_SIGNATURE_MAXIMUM_CHARACTERS: i32 = 1_024;
const SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS: i32 = 600;
const ROLE_EVIDENCE_MAXIMUM_CHARACTERS: i32 = 2_048;
const DEFAULT_SUMMARY_MINIMUM_BODY_LINES: u32 = 4;
const DEFAULT_SUMMARY_DOCSTRING_THRESHOLD: u32 = 20;
const MAX_SUMMARY_MINIMUM_BODY_LINES: u32 = 1_000_000;
const MAX_SUMMARY_KIND_OVERRIDES: usize = 128;
const MAX_SUMMARY_KIND_BYTES: usize = 64;

/// Exact candidate policy shared by summary backlog and readiness queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryCandidatePolicy {
    minimum_body_lines: u32,
    minimum_body_lines_by_kind: BTreeMap<String, u32>,
    existing_docstring_char_threshold: u32,
}

impl Default for SummaryCandidatePolicy {
    fn default() -> Self {
        Self {
            minimum_body_lines: DEFAULT_SUMMARY_MINIMUM_BODY_LINES,
            minimum_body_lines_by_kind: BTreeMap::from([("route".to_owned(), 1)]),
            existing_docstring_char_threshold: DEFAULT_SUMMARY_DOCSTRING_THRESHOLD,
        }
    }
}

impl SummaryCandidatePolicy {
    /// Creates a validated summary candidate policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the default line floor is too large, there are too
    /// many kind overrides, or an override kind/floor violates its bound.
    pub fn new(
        minimum_body_lines: u32,
        minimum_body_lines_by_kind: BTreeMap<String, u32>,
    ) -> Result<Self, StorageError> {
        if minimum_body_lines > MAX_SUMMARY_MINIMUM_BODY_LINES
            || minimum_body_lines_by_kind.len() > MAX_SUMMARY_KIND_OVERRIDES
            || minimum_body_lines_by_kind
                .iter()
                .any(|(kind, floor)| !valid_summary_kind_override(kind, *floor))
        {
            return Err(StorageError::InvalidInput {
                field: "summary_candidate_policy",
            });
        }
        Ok(Self {
            minimum_body_lines,
            minimum_body_lines_by_kind,
            existing_docstring_char_threshold: DEFAULT_SUMMARY_DOCSTRING_THRESHOLD,
        })
    }

    fn minimum_body_lines_json(&self) -> Result<String, StorageError> {
        serde_json::to_string(&self.minimum_body_lines_by_kind).map_err(|_| {
            StorageError::InvalidInput {
                field: "summary_candidate_policy",
            }
        })
    }
}

fn valid_summary_kind_override(kind: &str, floor: u32) -> bool {
    !kind.is_empty()
        && kind.len() <= MAX_SUMMARY_KIND_BYTES
        && !kind.chars().any(char::is_control)
        && floor <= MAX_SUMMARY_MINIMUM_BODY_LINES
}

/// Durable agent-memory family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentArtifactKind {
    /// Free-form bounded agent note.
    Note,
    /// Inferred symbol-role artifact.
    Role,
    /// Model or structural summary artifact.
    Summary,
    /// Artifact owned by an agent session.
    Session,
}

impl AgentArtifactKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Role => "role",
            Self::Summary => "summary",
            Self::Session => "session",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "note" => Some(Self::Note),
            "role" => Some(Self::Role),
            "summary" => Some(Self::Summary),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

/// Stable scope axis for notes, summaries, roles, and sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentArtifactScope {
    /// Artifact applies to the complete project.
    Project,
    /// Artifact applies to one normalized module.
    Module,
    /// Artifact applies to one indexed file.
    File,
    /// Artifact applies to one indexed symbol.
    Symbol,
    /// Artifact applies to one agent session.
    Session,
}

impl AgentArtifactScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Module => "module",
            Self::File => "file",
            Self::Symbol => "symbol",
            Self::Session => "session",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "project" => Some(Self::Project),
            "module" => Some(Self::Module),
            "file" => Some(Self::File),
            "symbol" => Some(Self::Symbol),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

/// Lifecycle for one durable artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentArtifactState {
    /// Represents the pending artifact state state.
    Pending,
    /// Represents the active artifact state state.
    Active,
    /// Represents the complete artifact state state.
    Complete,
    /// Represents the stale artifact state state.
    Stale,
    /// Represents the archived artifact state state.
    Archived,
}

impl AgentArtifactState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Complete => "complete",
            Self::Stale => "stale",
            Self::Archived => "archived",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "complete" => Some(Self::Complete),
            "stale" => Some(Self::Stale),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }
}

/// Validated write payload for a PostgreSQL-backed agent artifact.
#[derive(Clone, Debug)]
pub struct NewAgentArtifact {
    kind: AgentArtifactKind,
    scope: AgentArtifactScope,
    scope_key: String,
    body: String,
    metadata: Value,
    generation_id: Option<GenerationId>,
    source_digest: Option<ContentDigest>,
    state: AgentArtifactState,
}

/// Bounded textual scope and body for a new durable agent artifact.
pub struct AgentArtifactContent {
    scope_key: String,
    body: String,
}

impl AgentArtifactContent {
    #[must_use]
    /// Creates the bounded scope key and body for a new artifact.
    pub fn new(scope_key: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            scope_key: scope_key.into(),
            body: body.into(),
        }
    }
}

impl NewAgentArtifact {
    /// Create a bounded active artifact without generation coupling.
    /// # Errors
    ///
    /// Returns an error if the scope key or active-state body is empty,
    /// oversized, or contains a NUL byte.
    pub fn new(
        kind: AgentArtifactKind,
        scope: AgentArtifactScope,
        content: AgentArtifactContent,
    ) -> Result<Self, StorageError> {
        let AgentArtifactContent { scope_key, body } = content;
        validate_scope_key(&scope_key)?;
        validate_body(&body, AgentArtifactState::Active)?;
        Ok(Self {
            kind,
            scope,
            scope_key,
            body,
            metadata: Value::Object(serde_json::Map::new()),
            generation_id: None,
            source_digest: None,
            state: AgentArtifactState::Active,
        })
    }

    /// Attach bounded JSON-object provenance.
    /// # Errors
    ///
    /// Returns an error if metadata is not a bounded serializable JSON object.
    pub fn with_metadata(mut self, metadata: Value) -> Result<Self, StorageError> {
        validate_metadata(&metadata)?;
        self.metadata = metadata;
        Ok(self)
    }

    /// Bind the artifact to one immutable generation.
    #[must_use]
    pub fn with_generation(mut self, generation_id: GenerationId) -> Self {
        self.generation_id = Some(generation_id);
        self
    }

    /// Record the source digest used to produce the artifact.
    #[must_use]
    pub fn with_source_digest(mut self, source_digest: ContentDigest) -> Self {
        self.source_digest = Some(source_digest);
        self
    }

    /// Select an explicit lifecycle state.
    /// # Errors
    ///
    /// Returns an error if the existing body is incompatible with the selected
    /// lifecycle state's emptiness or byte-length contract.
    pub fn with_state(mut self, state: AgentArtifactState) -> Result<Self, StorageError> {
        validate_body(&self.body, state)?;
        self.state = state;
        Ok(self)
    }
}

/// Bounded durable artifact listing filters.
#[derive(Clone, Copy, Debug)]
pub struct AgentArtifactQuery<'query> {
    kind: Option<AgentArtifactKind>,
    scope: Option<AgentArtifactScope>,
    scope_key: Option<&'query str>,
    body_equals: Option<&'query str>,
    note_kind: Option<&'query str>,
    since_unix_ms: Option<f64>,
    current_generation_only: bool,
    state: Option<AgentArtifactState>,
    limit: u16,
}

impl<'query> AgentArtifactQuery<'query> {
    /// Create an unfiltered bounded listing.
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or exceeds the artifact listing cap.
    pub fn new(limit: u16) -> Result<Self, StorageError> {
        validate_limit(limit, MAX_ARTIFACT_LIMIT)?;
        Ok(Self {
            kind: None,
            scope: None,
            scope_key: None,
            body_equals: None,
            note_kind: None,
            since_unix_ms: None,
            current_generation_only: false,
            state: None,
            limit,
        })
    }

    #[must_use]
    /// Sets the kind and returns the updated value.
    pub const fn with_kind(mut self, kind: AgentArtifactKind) -> Self {
        self.kind = Some(kind);
        self
    }

    #[must_use]
    /// Sets the scope and returns the updated value.
    pub const fn with_scope(mut self, scope: AgentArtifactScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Sets the scope key and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if `scope_key` is empty, oversized, or contains a NUL byte.
    pub fn with_scope_key(mut self, scope_key: &'query str) -> Result<Self, StorageError> {
        validate_scope_key(scope_key)?;
        self.scope_key = Some(scope_key);
        Ok(self)
    }

    /// Filter exact bounded body labels, used for role listings.
    /// # Errors
    ///
    /// Returns an error if `body` is empty, exceeds the exact-label byte cap,
    /// or contains a NUL byte.
    pub fn with_body(mut self, body: &'query str) -> Result<Self, StorageError> {
        if body.is_empty() || body.len() > 256 || body.contains('\0') {
            return Err(StorageError::InvalidInput {
                field: "artifact_body_filter",
            });
        }
        self.body_equals = Some(body);
        Ok(self)
    }

    /// Filter a note subtype stored in validated metadata.
    /// # Errors
    ///
    /// Returns an error unless `note_kind` is `note`, `question`, `followup`,
    /// or `bookmark`.
    pub fn with_note_kind(mut self, note_kind: &'query str) -> Result<Self, StorageError> {
        if !matches!(note_kind, "note" | "question" | "followup" | "bookmark") {
            return Err(StorageError::InvalidInput { field: "note_kind" });
        }
        self.note_kind = Some(note_kind);
        Ok(self)
    }

    /// Restrict results to artifacts written at or after a Unix-millisecond instant.
    /// # Errors
    ///
    /// Returns an error if the Unix-millisecond value is negative or non-finite.
    pub fn since_unix_ms(mut self, since_unix_ms: f64) -> Result<Self, StorageError> {
        if !since_unix_ms.is_finite() || since_unix_ms < 0.0 {
            return Err(StorageError::InvalidInput { field: "since" });
        }
        self.since_unix_ms = Some(since_unix_ms);
        Ok(self)
    }

    /// Restrict generation-coupled artifacts to the published generation.
    #[must_use]
    pub const fn current_generation_only(mut self) -> Self {
        self.current_generation_only = true;
        self
    }

    #[must_use]
    /// Sets the state and returns the updated value.
    pub const fn with_state(mut self, state: AgentArtifactState) -> Self {
        self.state = Some(state);
        self
    }
}

/// One durable PostgreSQL-backed artifact.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentArtifactRecord {
    id: u64,
    artifact_id: String,
    kind: AgentArtifactKind,
    scope: AgentArtifactScope,
    scope_key: String,
    body: String,
    metadata: Value,
    generation_id: Option<String>,
    source_digest: Option<String>,
    state: AgentArtifactState,
    created_at: String,
    updated_at: String,
}

/// One symbol whose structural digest has no matching complete summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingSummarySymbol {
    generation_id: String,
    symbol_id: String,
    path: String,
    language: String,
    symbol_kind: String,
    qualified_name: String,
    signature: String,
    start_line: u32,
    end_line: u32,
    content_hash: String,
    code: String,
    code_truncated: bool,
    priority: bool,
}

/// One bounded outgoing edge used by deterministic summary rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralSummaryEdge {
    edge_kind: String,
    target_symbol_id: String,
    target_kind: String,
    target_name: String,
    target_qualified_name: String,
    target_path: String,
    target_summary: Option<String>,
}

/// One current thin symbol whose exact structural digest has no cached summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingStructuralSummary {
    generation_id: String,
    symbol_id: String,
    path: String,
    symbol_kind: String,
    name: String,
    qualified_name: String,
    signature: String,
    code: String,
    start_line: u32,
    end_line: u32,
    content_hash: String,
    declaration_only: bool,
    edges: Vec<StructuralSummaryEdge>,
}

/// One current embedded symbol with no digest-compatible summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingNeighborSummary {
    generation_id: String,
    symbol_id: String,
    symbol_kind: String,
    name: String,
    content_hash: String,
}

/// One exact non-propagated current summary eligible as neighbor evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NeighborSummarySource {
    symbol_id: String,
    symbol_kind: String,
    name: String,
    summary: String,
    model: String,
}

/// One summarized symbol used as bounded evidence for a file roll-up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRollupItem {
    symbol_id: String,
    qualified_name: String,
    symbol_kind: String,
    summary: String,
}

/// One current file whose symbol-summary roll-up is missing or stale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingFileSummary {
    generation_id: String,
    path: String,
    file_content_hash: String,
    content_hash: String,
    summarized_symbols: u64,
    items: Vec<SummaryRollupItem>,
    items_truncated: bool,
}

/// Bounded, generation-current file or module summary roll-up query.
pub struct PendingSummaryRollupQuery<'input> {
    project_id: &'input ProjectId,
    model: &'input str,
    anchor_digest: &'input ContentDigest,
    after: Option<&'input str>,
    limit: u16,
    item_limit: u16,
}

impl<'input> PendingSummaryRollupQuery<'input> {
    /// Bind the project, model, and prompt anchor used to validate cached summaries.
    #[must_use]
    pub const fn new(
        project_id: &'input ProjectId,
        model: &'input str,
        anchor_digest: &'input ContentDigest,
    ) -> Self {
        Self {
            project_id,
            model,
            anchor_digest,
            after: None,
            limit: 0,
            item_limit: 0,
        }
    }

    /// Continue after one validated file path or module directory.
    #[must_use]
    pub const fn after(mut self, after: Option<&'input str>) -> Self {
        self.after = after;
        self
    }

    /// Apply the result-page and per-roll-up evidence bounds.
    #[must_use]
    pub const fn page(mut self, limit: u16, item_limit: u16) -> Self {
        self.limit = limit;
        self.item_limit = item_limit;
        self
    }
}

/// Digest-fenced file-summary publication request.
pub struct FileSummarySaveRequest<'a> {
    path: &'a NormalizedPath,
    fields: SummarySaveFields<'a>,
}

/// Exact semantic-neighbor publication request. The database revalidates the
/// source summary, same-kind relation, current generation, and target digest.
pub struct NeighborSummarySaveRequest<'a> {
    symbol_id: &'a SymbolId,
    source_digest: &'a ContentDigest,
    neighbor_symbol_id: &'a SymbolId,
    neighbor_summary: &'a str,
    summary: &'a str,
    embedding_model_id: &'a ModelId,
    similarity: f64,
}

#[derive(Clone, Copy)]
/// Validated and bounded neighbor summary save input.
pub struct NeighborSummarySaveInput<'a> {
    /// Stable symbol ID for this record.
    pub symbol_id: &'a SymbolId,
    /// Digest-fenced source digest for this record.
    pub source_digest: &'a ContentDigest,
    /// Stable neighbor symbol ID for this record.
    pub neighbor_symbol_id: &'a SymbolId,
    /// Neighbor summary for this record.
    pub neighbor_summary: &'a str,
    /// Summary for this record.
    pub summary: &'a str,
    /// Stable embedding model ID for this record.
    pub embedding_model_id: &'a ModelId,
    /// Similarity for this record.
    pub similarity: f64,
}

impl<'a> NeighborSummarySaveRequest<'a> {
    /// Creates a validated neighbor summary save request.
    ///
    /// # Errors
    ///
    /// Returns an error if source and neighbor are identical, similarity is
    /// non-finite/outside zero to one, or the complete summary body is invalid.
    pub fn new(input: NeighborSummarySaveInput<'a>) -> Result<Self, StorageError> {
        let NeighborSummarySaveInput {
            symbol_id,
            source_digest,
            neighbor_symbol_id,
            neighbor_summary,
            summary,
            embedding_model_id,
            similarity,
        } = input;
        if symbol_id == neighbor_symbol_id
            || !similarity.is_finite()
            || !(0.0..=1.0).contains(&similarity)
        {
            return Err(StorageError::InvalidInput {
                field: "neighbor_summary",
            });
        }
        validate_body(summary, AgentArtifactState::Complete)?;
        Ok(Self {
            symbol_id,
            source_digest,
            neighbor_symbol_id,
            neighbor_summary,
            summary,
            embedding_model_id,
            similarity,
        })
    }
}

/// One summarized symbol used as bounded evidence for a directory roll-up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleSummaryRollupItem {
    symbol_id: String,
    path: String,
    qualified_name: String,
    symbol_kind: String,
    summary: String,
}

/// One immediate source directory whose module paragraph is missing or stale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingModuleSummary {
    generation_id: String,
    directory: String,
    content_hash: String,
    summarized_symbols: u64,
    tool_export_constants: u64,
    items: Vec<ModuleSummaryRollupItem>,
    items_truncated: bool,
}

/// Digest-fenced module-summary publication request.
pub struct ModuleSummarySaveRequest<'a> {
    directory: &'a NormalizedPath,
    fields: SummarySaveFields<'a>,
}

#[derive(Clone, Copy)]
struct SummarySaveFields<'a> {
    expected_digest: &'a ContentDigest,
    anchor_digest: &'a ContentDigest,
    summary: &'a str,
    model: &'a str,
    item_limit: u16,
    generation_mode: &'static str,
}

/// Shared digest fence, text, model, and evidence bound for a file or module summary write.
#[derive(Clone, Copy)]
pub struct SummarySaveInput<'a> {
    expected_digest: &'a ContentDigest,
    anchor_digest: &'a ContentDigest,
    summary: &'a str,
    model: &'a str,
    item_limit: u16,
}

impl<'a> SummarySaveInput<'a> {
    #[must_use]
    /// Creates a validated summary save input.
    pub const fn new(
        expected_digest: &'a ContentDigest,
        anchor_digest: &'a ContentDigest,
        summary: &'a str,
    ) -> Self {
        Self {
            expected_digest,
            anchor_digest,
            summary,
            model: "",
            item_limit: 0,
        }
    }

    #[must_use]
    /// Sets the model and returns the updated value.
    pub const fn with_model(mut self, model: &'a str) -> Self {
        self.model = model;
        self
    }

    #[must_use]
    /// Sets the item limit and returns the updated value.
    pub const fn with_item_limit(mut self, item_limit: u16) -> Self {
        self.item_limit = item_limit;
        self
    }
}

/// Validated and bounded pending structural summary query.
pub struct PendingStructuralSummaryQuery<'a> {
    project_id: &'a ProjectId,
    after_symbol_id: Option<&'a SymbolId>,
    limit: u16,
    policy: &'a SummaryCandidatePolicy,
}

impl<'a> PendingStructuralSummaryQuery<'a> {
    #[must_use]
    /// Creates a validated pending structural summary query.
    pub const fn new(
        project_id: &'a ProjectId,
        limit: u16,
        policy: &'a SummaryCandidatePolicy,
    ) -> Self {
        Self {
            project_id,
            after_symbol_id: None,
            limit,
            policy,
        }
    }

    #[must_use]
    /// Returns the after symbol.
    pub const fn after_symbol(mut self, symbol_id: Option<&'a SymbolId>) -> Self {
        self.after_symbol_id = symbol_id;
        self
    }
}

/// Validated and bounded pending neighbor summary query.
pub struct PendingNeighborSummaryQuery<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    model_id: &'a ModelId,
    after_symbol_id: Option<&'a SymbolId>,
    limit: u16,
}

impl<'a> PendingNeighborSummaryQuery<'a> {
    #[must_use]
    /// Creates a validated pending neighbor summary query.
    pub const fn new(
        project_id: &'a ProjectId,
        expected_generation_id: &'a GenerationId,
        model_id: &'a ModelId,
    ) -> Self {
        Self {
            project_id,
            expected_generation_id,
            model_id,
            after_symbol_id: None,
            limit: 0,
        }
    }

    #[must_use]
    /// Returns the after symbol.
    pub const fn after_symbol(mut self, symbol_id: Option<&'a SymbolId>) -> Self {
        self.after_symbol_id = symbol_id;
        self
    }

    #[must_use]
    /// Sets the limit and returns the updated value.
    pub const fn with_limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }
}

/// Validated and bounded pending model summary query.
pub struct PendingModelSummaryQuery<'a> {
    project_id: &'a ProjectId,
    model: &'a str,
    limit: u16,
    policy: &'a SummaryCandidatePolicy,
}

impl<'a> PendingModelSummaryQuery<'a> {
    #[must_use]
    /// Creates a validated pending model summary query.
    pub const fn new(
        project_id: &'a ProjectId,
        model: &'a str,
        policy: &'a SummaryCandidatePolicy,
    ) -> Self {
        Self {
            project_id,
            model,
            limit: 0,
            policy,
        }
    }

    #[must_use]
    /// Sets the limit and returns the updated value.
    pub const fn with_limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }
}

/// Validated and bounded symbol summary save input.
pub struct SymbolSummarySaveInput<'a> {
    project_id: &'a ProjectId,
    symbol_id: &'a SymbolId,
    source_digest: &'a ContentDigest,
    summary: &'a str,
    model: &'a str,
}

impl<'a> SymbolSummarySaveInput<'a> {
    #[must_use]
    /// Creates a validated symbol summary save input.
    pub const fn new(
        project_id: &'a ProjectId,
        symbol_id: &'a SymbolId,
        source_digest: &'a ContentDigest,
    ) -> Self {
        Self {
            project_id,
            symbol_id,
            source_digest,
            summary: "",
            model: "",
        }
    }

    #[must_use]
    /// Sets the summary and returns the updated value.
    pub const fn with_summary(mut self, summary: &'a str) -> Self {
        self.summary = summary;
        self
    }

    #[must_use]
    /// Sets the model and returns the updated value.
    pub const fn with_model(mut self, model: &'a str) -> Self {
        self.model = model;
        self
    }
}

/// Validated and bounded structural symbol summary save input.
pub struct StructuralSymbolSummarySaveInput<'a> {
    project_id: &'a ProjectId,
    symbol_id: &'a SymbolId,
    source_digest: &'a ContentDigest,
    summary: &'a str,
}

impl<'a> StructuralSymbolSummarySaveInput<'a> {
    #[must_use]
    /// Creates a validated structural symbol summary save input.
    pub const fn new(
        project_id: &'a ProjectId,
        symbol_id: &'a SymbolId,
        source_digest: &'a ContentDigest,
    ) -> Self {
        Self {
            project_id,
            symbol_id,
            source_digest,
            summary: "",
        }
    }

    #[must_use]
    /// Sets the summary and returns the updated value.
    pub const fn with_summary(mut self, summary: &'a str) -> Self {
        self.summary = summary;
        self
    }
}

/// Validated and bounded symbol role save input.
pub struct SymbolRoleSaveInput<'a> {
    project_id: &'a ProjectId,
    symbol_id: &'a SymbolId,
    role: &'a str,
    metadata: Value,
}

impl<'a> SymbolRoleSaveInput<'a> {
    #[must_use]
    /// Creates a validated symbol role save input.
    pub const fn new(project_id: &'a ProjectId, symbol_id: &'a SymbolId, role: &'a str) -> Self {
        Self {
            project_id,
            symbol_id,
            role,
            metadata: Value::Null,
        }
    }

    #[must_use]
    /// Sets the metadata and returns the updated value.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

struct PendingSymbolSummaryRequest<'a> {
    project_id: &'a ProjectId,
    model: Option<&'a str>,
    limit: u16,
    policy: &'a SummaryCandidatePolicy,
}

struct ArtifactReadRequest<'a> {
    statement: String,
    project_id: &'a ProjectId,
    operation: &'static str,
}

#[derive(Clone, Copy)]
struct SummaryRollupDigestInput<'a> {
    domain: &'a str,
    scope_key: &'a str,
    source_hash: &'a str,
    total_items: u64,
    items: &'a [SummaryRollupItem],
    anchor_digest: &'a ContentDigest,
}

#[derive(Clone, Copy)]
struct ModuleSummaryRollupDigestInput<'a> {
    directory: &'a str,
    total_items: u64,
    tool_export_constants: u64,
    items: &'a [ModuleSummaryRollupItem],
    anchor_digest: &'a ContentDigest,
}

fn validate_summary_generation_mode(generation_mode: &str) -> Result<(), StorageError> {
    if matches!(generation_mode, "llm" | "structural_rule") {
        Ok(())
    } else {
        Err(StorageError::InvalidInput {
            field: "summary_generation_mode",
        })
    }
}

impl<'a> SummarySaveFields<'a> {
    fn new(input: SummarySaveInput<'a>, maximum_items: u16) -> Result<Self, StorageError> {
        validate_body(input.summary, AgentArtifactState::Complete)?;
        validate_model(input.model)?;
        validate_limit(input.item_limit, maximum_items)?;
        Ok(Self {
            expected_digest: input.expected_digest,
            anchor_digest: input.anchor_digest,
            summary: input.summary,
            model: input.model,
            item_limit: input.item_limit,
            generation_mode: "llm",
        })
    }

    fn with_generation_mode(mut self, generation_mode: &'static str) -> Result<Self, StorageError> {
        validate_summary_generation_mode(generation_mode)?;
        self.generation_mode = generation_mode;
        Ok(self)
    }
}

impl<'a> ModuleSummarySaveRequest<'a> {
    /// Creates a validated module summary save request.
    ///
    /// # Errors
    ///
    /// Returns an error if model/body/digest/evidence fields or the module
    /// roll-up item count violate summary-save bounds.
    pub fn new(
        directory: &'a NormalizedPath,
        input: SummarySaveInput<'a>,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            directory,
            fields: SummarySaveFields::new(input, MAX_MODULE_SUMMARY_ROLLUP_ITEMS)?,
        })
    }

    /// Sets the generation mode and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error unless `generation_mode` is a recognized bounded
    /// summary-generation provenance value.
    pub fn with_generation_mode(self, generation_mode: &'static str) -> Result<Self, StorageError> {
        validate_summary_generation_mode(generation_mode)?;
        Ok(Self {
            fields: self.fields.with_generation_mode(generation_mode)?,
            ..self
        })
    }
}

impl<'a> FileSummarySaveRequest<'a> {
    /// Creates a validated file summary save request.
    ///
    /// # Errors
    ///
    /// Returns an error if model/body/digest/evidence fields or the file
    /// roll-up item count violate summary-save bounds.
    pub fn new(
        path: &'a NormalizedPath,
        input: SummarySaveInput<'a>,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            path,
            fields: SummarySaveFields::new(input, MAX_SUMMARY_ROLLUP_ITEMS)?,
        })
    }

    /// Sets the generation mode and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error unless `generation_mode` is a recognized bounded
    /// summary-generation provenance value.
    pub fn with_generation_mode(
        mut self,
        generation_mode: &'static str,
    ) -> Result<Self, StorageError> {
        self.fields = self.fields.with_generation_mode(generation_mode)?;
        Ok(self)
    }
}

/// Which role sweep asks for pending symbols, and with what model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleSweepModel<'a> {
    /// Rules only. Roles a model already judged are kept, so turning a model
    /// off never downgrades its paid results.
    Structural(&'a str),
    /// A chat or Jev model re-judges every symbol it has not judged itself,
    /// except structural rule matches.
    Judge(&'a str),
}

/// One current symbol without a digest/model-compatible role classification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRoleSymbol {
    symbol_id: String,
    path: String,
    language: String,
    symbol_kind: String,
    qualified_name: String,
    signature: String,
    description: String,
    code: String,
    exported: bool,
}

/// Persisted role-classification count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRoleCount {
    role: String,
    symbols: u64,
}

/// Exact current-generation summary coverage with durable model provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryCoverageStats {
    eligible_symbols: u64,
    summarized_symbols: u64,
    pending_symbols: u64,
    by_model: BTreeMap<String, u64>,
}

/// One current cached module paragraph.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModuleSummary {
    directory: String,
    summary: String,
    metadata: Value,
}

/// Exact module-summary total plus one bounded path-ordered page.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModuleSummaryPage {
    summaries: Vec<CurrentModuleSummary>,
    total: u64,
    truncated: bool,
}

impl CurrentModuleSummaryPage {
    #[must_use]
    /// Returns the summaries.
    pub fn summaries(&self) -> &[CurrentModuleSummary] {
        &self.summaries
    }

    #[must_use]
    /// Returns the total.
    pub const fn total(&self) -> u64 {
        self.total
    }

    #[must_use]
    /// Whether the page limit omitted additional module summaries.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl CurrentModuleSummary {
    #[must_use]
    /// Returns the directory.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    #[must_use]
    /// Returns the summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    /// Returns the metadata.
    pub const fn metadata(&self) -> &Value {
        &self.metadata
    }
}

impl AgentArtifactRecord {
    /// Monotonic project-visible identity retained for v1 note compatibility.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Stable public artifact identity used by delete/resume workflows.
    #[must_use]
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    /// Artifact scope key, such as a symbol UUID or session label.
    #[must_use]
    pub fn scope_key(&self) -> &str {
        &self.scope_key
    }

    /// Bounded artifact content.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Structured provenance and subtype metadata.
    #[must_use]
    pub const fn metadata(&self) -> &Value {
        &self.metadata
    }
}

impl PendingSummarySymbol {
    /// Exact current-generation symbol identity.
    #[must_use]
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    /// Structural digest that must be echoed when saving a summary.
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Exact immutable generation containing this symbol.
    #[must_use]
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    /// Project-relative path used as summary context.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Indexed source language.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Extracted symbol kind.
    #[must_use]
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    /// Extracted qualified name.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Bounded extracted declaration signature.
    #[must_use]
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// Bounded indexed implementation evidence, never live working-tree text.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Whether the indexed implementation exceeded the prompt-safe excerpt.
    #[must_use]
    pub const fn code_truncated(&self) -> bool {
        self.code_truncated
    }

    /// Whether a no-hit intent lookup explicitly requested this symbol.
    #[must_use]
    pub const fn priority(&self) -> bool {
        self.priority
    }
}

impl StructuralSummaryEdge {
    #[must_use]
    /// Returns the edge kind.
    pub fn edge_kind(&self) -> &str {
        &self.edge_kind
    }

    #[must_use]
    /// Returns the target symbol ID.
    pub fn target_symbol_id(&self) -> &str {
        &self.target_symbol_id
    }

    #[must_use]
    /// Returns the target kind.
    pub fn target_kind(&self) -> &str {
        &self.target_kind
    }

    #[must_use]
    /// Returns the target name.
    pub fn target_name(&self) -> &str {
        &self.target_name
    }

    #[must_use]
    /// Returns the target qualified name.
    pub fn target_qualified_name(&self) -> &str {
        &self.target_qualified_name
    }

    #[must_use]
    /// Returns the target path.
    pub fn target_path(&self) -> &str {
        &self.target_path
    }

    #[must_use]
    /// Returns the target summary.
    pub fn target_summary(&self) -> Option<&str> {
        self.target_summary.as_deref()
    }
}

impl PendingStructuralSummary {
    #[must_use]
    /// Returns the generation ID.
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the symbol kind.
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    #[must_use]
    /// Returns the name.
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    /// Returns the qualified name.
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    #[must_use]
    /// Returns the signature.
    pub fn signature(&self) -> &str {
        &self.signature
    }

    #[must_use]
    /// Returns the code.
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    /// Returns the start line.
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    #[must_use]
    /// Returns the end line.
    pub const fn end_line(&self) -> u32 {
        self.end_line
    }

    #[must_use]
    /// Returns the content hash.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    #[must_use]
    /// Returns the declaration only.
    pub const fn declaration_only(&self) -> bool {
        self.declaration_only
    }

    #[must_use]
    /// Returns the edges.
    pub fn edges(&self) -> &[StructuralSummaryEdge] {
        &self.edges
    }
}

impl PendingNeighborSummary {
    #[must_use]
    /// Returns the generation ID.
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the symbol kind.
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    #[must_use]
    /// Returns the name.
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    /// Returns the content hash.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }
}

impl NeighborSummarySource {
    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the symbol kind.
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    #[must_use]
    /// Returns the name.
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    /// Returns the summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    /// Returns the model.
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl PendingFileSummary {
    #[must_use]
    /// Returns the generation ID.
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the content hash.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    #[must_use]
    /// Returns the file content hash.
    pub fn file_content_hash(&self) -> &str {
        &self.file_content_hash
    }

    #[must_use]
    /// Returns the summarized symbols.
    pub const fn summarized_symbols(&self) -> u64 {
        self.summarized_symbols
    }

    #[must_use]
    /// Returns the items.
    pub fn items(&self) -> &[SummaryRollupItem] {
        &self.items
    }

    #[must_use]
    /// Returns whether additional roll-up evidence items were omitted.
    pub const fn items_truncated(&self) -> bool {
        self.items_truncated
    }
}

impl PendingModuleSummary {
    #[must_use]
    /// Returns the generation ID.
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    #[must_use]
    /// Returns the directory.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    #[must_use]
    /// Returns the content hash.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    #[must_use]
    /// Returns the summarized symbols.
    pub const fn summarized_symbols(&self) -> u64 {
        self.summarized_symbols
    }

    #[must_use]
    /// Returns the tool export constants.
    pub const fn tool_export_constants(&self) -> u64 {
        self.tool_export_constants
    }

    #[must_use]
    /// Returns the items.
    pub fn items(&self) -> &[ModuleSummaryRollupItem] {
        &self.items
    }

    #[must_use]
    /// Returns whether additional roll-up evidence items were omitted.
    pub const fn items_truncated(&self) -> bool {
        self.items_truncated
    }
}

impl ModuleSummaryRollupItem {
    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the qualified name.
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    #[must_use]
    /// Returns the symbol kind.
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    #[must_use]
    /// Returns the summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }
}

impl SummaryRollupItem {
    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the qualified name.
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    #[must_use]
    /// Returns the symbol kind.
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    #[must_use]
    /// Returns the summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }
}

impl PendingRoleSymbol {
    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the language.
    pub fn language(&self) -> &str {
        &self.language
    }

    #[must_use]
    /// Returns the symbol kind.
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    #[must_use]
    /// Returns the qualified name.
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    #[must_use]
    /// Returns the signature.
    pub fn signature(&self) -> &str {
        &self.signature
    }

    #[must_use]
    /// Returns the description.
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    /// Returns the code.
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    /// Performs the bounded exported operation.
    pub const fn exported(&self) -> bool {
        self.exported
    }
}

impl CartographDatabase {
    /// Append a note/session artifact. Roles and summaries use replacement semantics below.
    /// # Errors
    ///
    /// Returns an error if artifact fields are invalid or PostgreSQL cannot
    /// insert and decode the project-scoped append-only record.
    pub async fn create_agent_artifact(
        &self,
        project_id: &ProjectId,
        artifact: &NewAgentArtifact,
    ) -> Result<AgentArtifactRecord, StorageError> {
        self.write_artifact(project_id, artifact, false).await
    }

    /// Replace the current role or summary for one exact scope.
    /// # Errors
    ///
    /// Returns an error unless the artifact is an unarchived role/summary with
    /// valid fields, or if its scoped upsert cannot be decoded.
    pub async fn replace_scoped_agent_artifact(
        &self,
        project_id: &ProjectId,
        artifact: &NewAgentArtifact,
    ) -> Result<AgentArtifactRecord, StorageError> {
        if !matches!(
            artifact.kind,
            AgentArtifactKind::Role | AgentArtifactKind::Summary
        ) || artifact.state == AgentArtifactState::Archived
        {
            return Err(StorageError::InvalidInput {
                field: "artifact_kind",
            });
        }
        self.write_artifact(project_id, artifact, true).await
    }

    async fn write_artifact(
        &self,
        project_id: &ProjectId,
        artifact: &NewAgentArtifact,
        replace_scope: bool,
    ) -> Result<AgentArtifactRecord, StorageError> {
        validate_artifact(artifact)?;
        let schema = quoted_schema(&self.schema);
        let conflict = if replace_scope {
            r"ON CONFLICT (project_id, artifact_kind, scope_kind, scope_key)
               WHERE artifact_kind IN ('role', 'summary') AND state <> 'archived'
               DO UPDATE SET body = EXCLUDED.body,
                             metadata = EXCLUDED.metadata,
                             generation_id = EXCLUDED.generation_id,
                             source_digest = EXCLUDED.source_digest,
                             state = EXCLUDED.state,
                             updated_at = clock_timestamp()"
        } else {
            ""
        };
        let statement = format!(
            r#"INSERT INTO {schema}."agent_artifacts" (
                    project_id, artifact_kind, scope_kind, scope_key, body, metadata,
                    generation_id, source_digest, state
                ) VALUES (
                    CAST($1 AS uuid), $2, $3, $4, $5, CAST($6 AS jsonb),
                    CAST($7 AS uuid), $8, $9
                )
                {conflict}
                RETURNING id, artifact_id::text, artifact_kind, scope_kind, scope_key,
                          body, metadata::text, generation_id::text, source_digest,
                          state, created_at::text, updated_at::text"#,
        );
        let metadata =
            serde_json::to_string(&artifact.metadata).map_err(|_| StorageError::InvalidInput {
                field: "artifact_metadata",
            })?;
        let row = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(artifact.kind.as_str())
            .bind(artifact.scope.as_str())
            .bind(&artifact.scope_key)
            .bind(&artifact.body)
            .bind(metadata)
            .bind(artifact.generation_id.as_ref().map(GenerationId::as_str))
            .bind(artifact.source_digest.as_ref().map(ContentDigest::as_str))
            .bind(artifact.state.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(|_| database_error("write-agent-artifact"))?;
        decode_artifact(&row)
    }

    /// List artifacts newest-first under bounded optional filters.
    /// # Errors
    ///
    /// Returns an error if filter/limit validation fails or a matching artifact
    /// row, metadata object, identity, or timestamp is malformed.
    pub async fn list_agent_artifacts(
        &self,
        project_id: &ProjectId,
        request: AgentArtifactQuery<'_>,
    ) -> Result<Vec<AgentArtifactRecord>, StorageError> {
        validate_limit(request.limit, MAX_ARTIFACT_LIMIT)?;
        if let Some(scope_key) = request.scope_key {
            validate_scope_key(scope_key)?;
        }
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT id, artifact_id::text, artifact_kind, scope_kind, scope_key,
                      body, metadata::text, generation_id::text, source_digest,
                      state, created_at::text, updated_at::text
                FROM {schema}."agent_artifacts"
                WHERE project_id = CAST($1 AS uuid)
                  AND ($2::text IS NULL OR artifact_kind = $2)
                  AND ($3::text IS NULL OR scope_kind = $3)
                  AND ($4::text IS NULL OR scope_key = $4)
                  AND ($5::text IS NULL OR state = $5)
                  AND ($6::text IS NULL OR body = $6)
                  AND ($7::text IS NULL OR metadata ->> 'noteKind' = $7)
                  AND ($8::double precision IS NULL OR updated_at >= to_timestamp($8 / 1000.0))
                  AND (
                      NOT $9::boolean
                      OR (
                          artifact_kind IN ('role', 'summary')
                          AND scope_kind = 'symbol'
                          AND source_digest IS NOT NULL
                          AND EXISTS (
                              SELECT 1
                              FROM {schema}."projects" AS projects
                              JOIN {schema}."symbols" AS symbols
                                ON symbols.project_id = projects.project_id
                               AND symbols.generation_id = projects.current_generation_id
                              WHERE projects.project_id = CAST($1 AS uuid)
                                AND symbols.symbol_id::text = scope_key
                                AND symbols.structural_digest = source_digest
                          )
                      )
                      OR (
                          artifact_kind = 'summary'
                          AND scope_kind IN ('file', 'module', 'project')
                          AND source_digest IS NOT NULL
                          AND generation_id = (
                              SELECT current_generation_id
                              FROM {schema}."projects"
                              WHERE project_id = CAST($1 AS uuid)
                          )
                      )
                      OR (
                          artifact_kind NOT IN ('role', 'summary')
                          AND generation_id = (
                              SELECT current_generation_id
                              FROM {schema}."projects"
                              WHERE project_id = CAST($1 AS uuid)
                          )
                      )
                  )
                ORDER BY updated_at DESC, id DESC
                LIMIT $10"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "list-agent-artifacts",
                },
                |statement| {
                    statement
                        .bind(request.kind.map(AgentArtifactKind::as_str))
                        .bind(request.scope.map(AgentArtifactScope::as_str))
                        .bind(request.scope_key)
                        .bind(request.state.map(AgentArtifactState::as_str))
                        .bind(request.body_equals)
                        .bind(request.note_kind)
                        .bind(request.since_unix_ms)
                        .bind(request.current_generation_only)
                        .bind(i64::from(request.limit))
                },
            )
            .await?;
        rows.iter().map(decode_artifact).collect()
    }

    /// Batch-read current file paragraphs for one small rendered listing.
    /// # Errors
    ///
    /// Returns an error if paths are empty/too numerous/non-normalized, JSON
    /// encoding fails, or a current file summary row is malformed.
    pub async fn current_file_summary_texts(
        &self,
        project_id: &ProjectId,
        paths: &[String],
    ) -> Result<BTreeMap<String, String>, StorageError> {
        if paths.is_empty() || paths.len() > MAX_INLINE_FILE_SUMMARIES {
            return Err(StorageError::InvalidInput {
                field: "file_summary_paths",
            });
        }
        for path in paths {
            NormalizedPath::parse(path).map_err(|_| StorageError::InvalidInput {
                field: "file_summary_paths",
            })?;
        }
        let encoded = serde_json::to_string(paths).map_err(|_| StorageError::InvalidInput {
            field: "file_summary_paths",
        })?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH requested AS (
                    SELECT jsonb_array_elements_text(CAST($2 AS jsonb)) AS path
                )
                SELECT artifacts.scope_key, artifacts.body
                FROM {schema}."agent_artifacts" AS artifacts
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = artifacts.project_id
                 AND projects.current_generation_id = artifacts.generation_id
                JOIN requested ON requested.path = artifacts.scope_key
                WHERE artifacts.project_id = CAST($1 AS uuid)
                  AND artifacts.artifact_kind = 'summary'
                  AND artifacts.scope_kind = 'file'
                  AND artifacts.state = 'complete'
                ORDER BY artifacts.scope_key"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "current-file-summary-texts",
                },
                |statement| statement.bind(encoded),
            )
            .await?;
        rows.iter()
            .map(|row| Ok((text(row, 0)?, text(row, 1)?)))
            .collect()
    }

    /// Read one exact module paragraph or a bounded path-ordered module page.
    /// # Errors
    ///
    /// Returns an error if `limit` is invalid or module summary body/metadata/
    /// count rows cannot be queried, parsed, or decoded.
    pub async fn current_module_summaries(
        &self,
        project_id: &ProjectId,
        directory: Option<&NormalizedPath>,
        limit: u16,
    ) -> Result<CurrentModuleSummaryPage, StorageError> {
        validate_limit(limit, MAX_ARTIFACT_LIMIT)?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT artifacts.scope_key, artifacts.body, artifacts.metadata::text,
                       COUNT(*) OVER ()::bigint
                FROM {schema}."agent_artifacts" AS artifacts
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = artifacts.project_id
                 AND projects.current_generation_id = artifacts.generation_id
                WHERE artifacts.project_id = CAST($1 AS uuid)
                  AND artifacts.artifact_kind = 'summary'
                  AND artifacts.scope_kind = 'module'
                  AND artifacts.state = 'complete'
                  AND ($2::text IS NULL OR artifacts.scope_key = $2)
                ORDER BY artifacts.scope_key
                LIMIT $3"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "current-module-summaries",
                },
                |statement| {
                    statement
                        .bind(directory.map(NormalizedPath::as_str))
                        .bind(i64::from(limit))
                },
            )
            .await?;
        let total = rows.first().map_or(Ok(0), |row| nonnegative_u64(row, 3))?;
        let summaries = rows
            .iter()
            .map(|row| {
                let metadata = serde_json::from_str(&text(row, 2)?).map_err(|_| {
                    StorageError::CorruptStoredValue {
                        field: "module_summary",
                    }
                })?;
                Ok(CurrentModuleSummary {
                    directory: text(row, 0)?,
                    summary: text(row, 1)?,
                    metadata,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CurrentModuleSummaryPage {
            truncated: total
                > u64::try_from(summaries.len()).map_err(|_| StorageError::CorruptStoredValue {
                    field: "module_summary",
                })?,
            summaries,
            total,
        })
    }

    /// Delete one exact project-owned artifact. Returns false when it was absent.
    /// # Errors
    ///
    /// Returns an error if `artifact_id` is not a canonical UUID or PostgreSQL
    /// cannot delete the exact project-owned row.
    pub async fn delete_agent_artifact(
        &self,
        project_id: &ProjectId,
        artifact_id: &str,
    ) -> Result<bool, StorageError> {
        if !canonical_uuid(artifact_id) {
            return Err(StorageError::InvalidInput {
                field: "artifact_id",
            });
        }
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"DELETE FROM {schema}."agent_artifacts"
                WHERE project_id = CAST($1 AS uuid)
                  AND artifact_id = CAST($2 AS uuid)"#,
        );
        let result = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(artifact_id)
            .execute(&self.pool)
            .await
            .map_err(|_| database_error("delete-agent-artifact"))?;
        Ok(result.rows_affected() == 1)
    }

    /// Delete one exact project-owned artifact by its monotonic compatibility identity.
    /// # Errors
    ///
    /// Returns an error if `artifact_id` is zero or not representable as a
    /// positive PostgreSQL `bigint`, or the scoped delete fails.
    pub async fn delete_agent_artifact_by_id(
        &self,
        project_id: &ProjectId,
        artifact_id: u64,
    ) -> Result<bool, StorageError> {
        let artifact_id = i64::try_from(artifact_id)
            .ok()
            .filter(|artifact_id| *artifact_id > 0)
            .ok_or(StorageError::InvalidInput {
                field: "artifact_id",
            })?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"DELETE FROM {schema}."agent_artifacts"
                WHERE project_id = CAST($1 AS uuid)
                  AND id = $2"#,
        );
        let result = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(artifact_id)
            .execute(&self.pool)
            .await
            .map_err(|_| database_error("delete-agent-artifact-by-id"))?;
        Ok(result.rows_affected() == 1)
    }

    /// Aggregate current persisted symbol-role labels without a row limit.
    /// # Errors
    ///
    /// Returns an error if current digest-compatible role labels/counts cannot
    /// be queried or a count is negative/malformed.
    pub async fn agent_role_distribution(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<AgentRoleCount>, StorageError> {
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT artifacts.body, COUNT(*)::bigint
                FROM {schema}."agent_artifacts" AS artifacts
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = artifacts.project_id
                JOIN {schema}."symbols" AS symbols
                  ON symbols.project_id = projects.project_id
                 AND symbols.generation_id = projects.current_generation_id
                 AND symbols.symbol_id::text = artifacts.scope_key
                 AND symbols.structural_digest = artifacts.source_digest
                WHERE artifacts.project_id = CAST($1 AS uuid)
                  AND artifacts.artifact_kind = 'role'
                  AND artifacts.scope_kind = 'symbol'
                  AND artifacts.state IN ('active', 'complete')
                GROUP BY artifacts.body
                ORDER BY COUNT(*) DESC, artifacts.body"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "agent-role-distribution",
                },
                |statement| statement,
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(AgentRoleCount {
                    role: text(row, 0)?,
                    symbols: nonnegative_u64(row, 1)?,
                })
            })
            .collect()
    }

    /// Aggregate summary coverage without a page-size blind spot.
    ///
    /// A summary counts only when it belongs to the visible generation and
    /// echoes the symbol's current structural digest. Stale artifacts remain
    /// durable for auditability but never inflate readiness.
    /// # Errors
    ///
    /// Returns an error if default candidate policy encoding fails or current
    /// eligible/matched summary aggregates cannot be queried or decoded.
    pub async fn current_summary_coverage(
        &self,
        project_id: &ProjectId,
    ) -> Result<SummaryCoverageStats, StorageError> {
        self.current_summary_coverage_with_policy(project_id, &SummaryCandidatePolicy::default())
            .await
    }

    /// Aggregate summary coverage under the exact configured candidate floor.
    /// # Errors
    ///
    /// Returns an error if kind-floor policy encoding fails or coverage counts
    /// and per-model JSON cannot be queried, parsed, or decoded.
    pub async fn current_summary_coverage_with_policy(
        &self,
        project_id: &ProjectId,
        policy: &SummaryCandidatePolicy,
    ) -> Result<SummaryCoverageStats, StorageError> {
        let body_lines_by_kind = policy.minimum_body_lines_json()?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH current AS (
                    SELECT current_generation_id AS generation_id
                    FROM {schema}."projects"
                    WHERE project_id = CAST($1 AS uuid)
                ), eligible AS (
                    SELECT symbols.symbol_id, symbols.generation_id,
                           symbols.structural_digest
                    FROM {schema}."symbols" AS symbols
                    JOIN current ON current.generation_id = symbols.generation_id
                    LEFT JOIN {schema}."search_documents" AS documents
                      ON documents.project_id = symbols.project_id
                     AND documents.generation_id = symbols.generation_id
                     AND documents.symbol_id = symbols.symbol_id
                     AND documents.document_kind = 'symbol'
                    WHERE symbols.project_id = CAST($1 AS uuid)
                      AND symbols.symbol_kind IN (
                          'class', 'function', 'method', 'interface', 'struct',
                          'trait', 'protocol', 'enum', 'type_alias', 'component', 'route'
                      )
                      AND (symbols.end_line - symbols.start_line + 1) >= COALESCE(
                          CAST(NULLIF(CAST($2 AS jsonb) ->> symbols.symbol_kind, '') AS integer),
                          $3
                      )
                      AND length(COALESCE(documents.natural_text, '')) < $4
                ), matched AS (
                    SELECT eligible.symbol_id, summaries.id,
                           COALESCE(NULLIF(summaries.metadata ->> 'model', ''), 'unknown') AS model
                    FROM eligible
                    LEFT JOIN {schema}."agent_artifacts" AS summaries
                      ON summaries.project_id = CAST($1 AS uuid)
                     AND summaries.artifact_kind = 'summary'
                     AND summaries.scope_kind = 'symbol'
                     AND summaries.scope_key = eligible.symbol_id::text
                     AND summaries.source_digest = eligible.structural_digest
                     AND summaries.state = 'complete'
                ), by_model AS (
                    SELECT model, COUNT(*)::bigint AS symbols
                    FROM matched
                    WHERE id IS NOT NULL
                    GROUP BY model
                )
                SELECT COUNT(*)::bigint,
                       COUNT(id)::bigint,
                       (COUNT(*) - COUNT(id))::bigint,
                       COALESCE((
                           SELECT jsonb_object_agg(model, symbols ORDER BY model)::text
                           FROM by_model
                       ), '{{}}')
                FROM matched"#,
        );
        let mut rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "current-summary-coverage",
                },
                |statement| {
                    statement
                        .bind(body_lines_by_kind)
                        .bind(i64::from(policy.minimum_body_lines))
                        .bind(i64::from(policy.existing_docstring_char_threshold))
                },
            )
            .await?;
        let row = rows.pop().ok_or(StorageError::CorruptStoredValue {
            field: "summary_coverage",
        })?;
        let by_model = serde_json::from_str(&text(&row, 3)?).map_err(|_| {
            StorageError::CorruptStoredValue {
                field: "summary_coverage",
            }
        })?;
        Ok(SummaryCoverageStats {
            eligible_symbols: nonnegative_u64(&row, 0)?,
            summarized_symbols: nonnegative_u64(&row, 1)?,
            pending_symbols: nonnegative_u64(&row, 2)?,
            by_model,
        })
    }

    /// Pull a deterministic bounded batch of missing or stale symbol summaries.
    /// # Errors
    ///
    /// Returns an error if `limit` or default policy encoding is invalid, or a
    /// missing/stale current symbol summary candidate cannot be decoded.
    pub async fn pending_symbol_summaries(
        &self,
        project_id: &ProjectId,
        limit: u16,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        self.pending_symbol_summaries_with_policy(
            project_id,
            limit,
            &SummaryCandidatePolicy::default(),
        )
        .await
    }

    /// Pull a bounded deterministic page for the no-model structural summary pass.
    ///
    /// A digest-compatible complete summary from any producer wins. This lets an
    /// LLM or agent race safely with the structural pass without being downgraded.
    /// # Errors
    ///
    /// Returns an error if the page bound is invalid or a digest-compatible
    /// structural candidate and bounded evidence cannot be queried or decoded.
    pub async fn pending_structural_summaries(
        &self,
        request: PendingStructuralSummaryQuery<'_>,
    ) -> Result<Vec<PendingStructuralSummary>, StorageError> {
        let PendingStructuralSummaryQuery {
            project_id,
            after_symbol_id,
            limit,
            policy,
        } = request;
        validate_limit(limit, MAX_STRUCTURAL_SUMMARY_BATCH)?;
        let schema = quoted_schema(&self.schema);
        let statement = include_str!("sql/artifacts_pending_structural_summaries.sql")
            .replace("{schema}", &schema);
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "pending-structural-summaries",
                },
                |statement| {
                    statement
                        .bind(after_symbol_id.map(SymbolId::as_str))
                        .bind(i64::from(limit))
                        .bind(SUMMARY_SIGNATURE_MAXIMUM_CHARACTERS)
                        .bind(SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS)
                        .bind(SUMMARY_SOURCE_MAXIMUM_CHARACTERS)
                        .bind(i64::from(policy.existing_docstring_char_threshold))
                },
            )
            .await?;
        rows.iter().map(decode_pending_structural_summary).collect()
    }

    /// Page current embedded symbols which still lack an exact summary.
    /// # Errors
    ///
    /// Returns an error if the page bound is invalid, the expected generation
    /// is stale, or an embedded unsummarized symbol row is malformed.
    pub async fn pending_neighbor_summaries(
        &self,
        request: PendingNeighborSummaryQuery<'_>,
    ) -> Result<Vec<PendingNeighborSummary>, StorageError> {
        let PendingNeighborSummaryQuery {
            project_id,
            expected_generation_id,
            model_id,
            after_symbol_id,
            limit,
        } = request;
        validate_limit(limit, MAX_NEIGHBOR_SUMMARY_BATCH)?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH current AS (
                    SELECT current_generation_id AS generation_id
                    FROM {schema}."projects"
                    WHERE project_id = CAST($1 AS uuid)
                      AND current_generation_id = CAST($2 AS uuid)
                )
                SELECT symbols.generation_id::text, symbols.symbol_id::text,
                       symbols.symbol_kind,
                       COALESCE(NULLIF(documents.metadata ->> 'name', ''),
                                symbols.qualified_name),
                       symbols.structural_digest
                FROM {schema}."symbols" AS symbols
                JOIN current ON current.generation_id = symbols.generation_id
                JOIN {schema}."search_documents" AS documents
                  ON documents.project_id = symbols.project_id
                 AND documents.generation_id = symbols.generation_id
                 AND documents.symbol_id = symbols.symbol_id
                JOIN {schema}."document_embeddings" AS embeddings
                  ON embeddings.project_id = documents.project_id
                 AND embeddings.generation_id = documents.generation_id
                 AND embeddings.document_id = documents.document_id
                 AND embeddings.model_id = CAST($3 AS uuid)
                LEFT JOIN {schema}."agent_artifacts" AS summaries
                  ON summaries.project_id = symbols.project_id
                 AND summaries.artifact_kind = 'summary'
                 AND summaries.scope_kind = 'symbol'
                 AND summaries.scope_key = symbols.symbol_id::text
                 AND summaries.state = 'complete'
                 AND summaries.source_digest = symbols.structural_digest
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.symbol_kind IN (
                      'class', 'function', 'method', 'interface', 'struct',
                      'trait', 'protocol', 'enum', 'type_alias', 'component', 'route'
                  )
                  AND summaries.id IS NULL
                  AND ($4::uuid IS NULL OR symbols.symbol_id > $4::uuid)
                ORDER BY symbols.symbol_id
                LIMIT $5"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "pending-neighbor-summaries",
                },
                |statement| {
                    statement
                        .bind(expected_generation_id.as_str())
                        .bind(model_id.as_str())
                        .bind(after_symbol_id.map(SymbolId::as_str))
                        .bind(i64::from(limit))
                },
            )
            .await?;
        rows.iter().map(decode_pending_neighbor_summary).collect()
    }

    /// Read exact current summaries for a bounded semantic-neighbor candidate set.
    /// Summaries already produced by neighbor propagation are excluded to prevent
    /// transitive semantic drift.
    /// # Errors
    ///
    /// Returns an error if the symbol set is empty/oversized, the generation is
    /// stale, or a non-propagated current summary source cannot be decoded.
    pub async fn neighbor_summary_sources(
        &self,
        project_id: &ProjectId,
        expected_generation_id: &GenerationId,
        symbol_ids: &[SymbolId],
    ) -> Result<Vec<NeighborSummarySource>, StorageError> {
        if symbol_ids.is_empty() || symbol_ids.len() > MAX_NEIGHBOR_SUMMARY_SOURCES {
            return Err(StorageError::InvalidInput {
                field: "neighbor_summary_sources",
            });
        }
        let mut ids = symbol_ids
            .iter()
            .map(|symbol_id| symbol_id.as_str().to_owned())
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH current AS (
                    SELECT current_generation_id AS generation_id
                    FROM {schema}."projects"
                    WHERE project_id = CAST($1 AS uuid)
                      AND current_generation_id = CAST($2 AS uuid)
                )
                SELECT symbols.symbol_id::text, symbols.symbol_kind,
                       COALESCE(NULLIF(documents.metadata ->> 'name', ''),
                                symbols.qualified_name),
                       summaries.body,
                       COALESCE(NULLIF(summaries.metadata ->> 'model', ''), 'agent')
                FROM {schema}."symbols" AS symbols
                JOIN current ON current.generation_id = symbols.generation_id
                JOIN {schema}."search_documents" AS documents
                  ON documents.project_id = symbols.project_id
                 AND documents.generation_id = symbols.generation_id
                 AND documents.symbol_id = symbols.symbol_id
                JOIN {schema}."agent_artifacts" AS summaries
                  ON summaries.project_id = symbols.project_id
                 AND summaries.artifact_kind = 'summary'
                 AND summaries.scope_kind = 'symbol'
                 AND summaries.scope_key = symbols.symbol_id::text
                 AND summaries.state = 'complete'
                 AND summaries.source_digest = symbols.structural_digest
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.symbol_id = ANY(CAST($3 AS uuid[]))
                  AND COALESCE(summaries.metadata ->> 'model', '') NOT LIKE 'neighbor:%'
                ORDER BY symbols.symbol_id"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "neighbor-summary-sources",
                },
                |statement| statement.bind(expected_generation_id.as_str()).bind(ids),
            )
            .await?;
        rows.iter().map(decode_neighbor_summary_source).collect()
    }

    /// Pull a deterministic bounded batch under the configured summary floor.
    /// # Errors
    ///
    /// Returns an error if `limit` or kind-floor policy encoding is invalid, or
    /// a digest-incompatible current summary candidate is malformed.
    pub async fn pending_symbol_summaries_with_policy(
        &self,
        project_id: &ProjectId,
        limit: u16,
        policy: &SummaryCandidatePolicy,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        self.pending_symbol_summaries_internal(PendingSymbolSummaryRequest {
            project_id,
            model: None,
            limit,
            policy,
        })
        .await
    }

    /// Pull candidates whose structural digest and exact model do not both match.
    /// # Errors
    ///
    /// Returns an error if model/limit/policy validation fails or a current
    /// candidate lacking that exact model/digest summary cannot be decoded.
    pub async fn pending_symbol_summaries_for_model_with_policy(
        &self,
        request: PendingModelSummaryQuery<'_>,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        let PendingModelSummaryQuery {
            project_id,
            model,
            limit,
            policy,
        } = request;
        validate_model(model)?;
        self.pending_symbol_summaries_internal(PendingSymbolSummaryRequest {
            project_id,
            model: Some(model),
            limit,
            policy,
        })
        .await
    }

    async fn pending_symbol_summaries_internal(
        &self,
        request: PendingSymbolSummaryRequest<'_>,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        let PendingSymbolSummaryRequest {
            project_id,
            model,
            limit,
            policy,
        } = request;
        validate_limit(limit, MAX_SUMMARY_BATCH)?;
        let body_lines_by_kind = policy.minimum_body_lines_json()?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH current AS (
                    SELECT current_generation_id AS generation_id
                    FROM {schema}."projects"
                    WHERE project_id = CAST($1 AS uuid)
                )
                SELECT symbols.generation_id::text AS generation_id,
                       symbols.symbol_id::text AS symbol_id,
                       files.normalized_path AS path,
                       files.language AS language,
                       symbols.symbol_kind AS symbol_kind,
                       symbols.qualified_name AS qualified_name,
                       left(symbols.signature, $3) AS signature,
                       symbols.start_line AS start_line,
                       symbols.end_line AS end_line,
                       symbols.structural_digest AS content_hash,
                       COALESCE(left(documents.code, $4), '') AS code,
                       COALESCE(length(documents.code) > $4, false) AS code_truncated,
                       priority.symbol_id IS NOT NULL AS priority
                FROM {schema}."symbols" AS symbols
                JOIN current ON current.generation_id = symbols.generation_id
                JOIN {schema}."files" AS files
                  ON files.project_id = symbols.project_id
                 AND files.generation_id = symbols.generation_id
                 AND files.file_id = symbols.file_id
                LEFT JOIN {schema}."search_documents" AS documents
                  ON documents.project_id = symbols.project_id
                 AND documents.generation_id = symbols.generation_id
                 AND documents.symbol_id = symbols.symbol_id
                 AND documents.document_kind = 'symbol'
                LEFT JOIN {schema}."agent_artifacts" AS summaries
                  ON summaries.project_id = symbols.project_id
                 AND summaries.artifact_kind = 'summary'
                 AND summaries.scope_kind = 'symbol'
                 AND summaries.scope_key = symbols.symbol_id::text
                 AND summaries.source_digest = symbols.structural_digest
                 AND summaries.state = 'complete'
                 AND (
                      ($8::text IS NULL AND COALESCE(summaries.metadata ->> 'model', '') NOT LIKE 'structural:%')
                      OR summaries.metadata ->> 'model' = $8
                 )
                LEFT JOIN {schema}."summary_priority_queue" AS priority
                  ON priority.project_id = symbols.project_id
                 AND priority.generation_id = symbols.generation_id
                 AND priority.symbol_id = symbols.symbol_id
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.symbol_kind IN (
                      'class', 'function', 'method', 'interface', 'struct',
                      'trait', 'protocol', 'enum', 'type_alias', 'component', 'route'
                  )
                  AND (symbols.end_line - symbols.start_line + 1) >= COALESCE(
                      CAST(NULLIF(CAST($5 AS jsonb) ->> symbols.symbol_kind, '') AS integer),
                      $6
                  )
                  AND length(COALESCE(documents.natural_text, '')) < $7
                  AND summaries.id IS NULL
                ORDER BY (priority.symbol_id IS NOT NULL) DESC,
                         priority.enqueued_at DESC NULLS LAST,
                         priority.requested_count DESC NULLS LAST,
                         symbols.exported DESC,
                         (symbols.visibility = 'public') DESC,
                         symbols.pagerank DESC NULLS LAST,
                         files.normalized_path,
                         symbols.start_line,
                         symbols.symbol_id
                LIMIT $2"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "pending-symbol-summaries",
                },
                |statement| {
                    statement
                        .bind(i64::from(limit))
                        .bind(SUMMARY_SIGNATURE_MAXIMUM_CHARACTERS)
                        .bind(SUMMARY_SOURCE_MAXIMUM_CHARACTERS)
                        .bind(body_lines_by_kind)
                        .bind(i64::from(policy.minimum_body_lines))
                        .bind(i64::from(policy.existing_docstring_char_threshold))
                        .bind(model)
                },
            )
            .await?;
        rows.iter().map(decode_pending_summary).collect()
    }

    /// Pull current files whose bounded symbol-summary roll-up has no exact
    /// model, source, anchor, and evidence-compatible cached paragraph.
    /// # Errors
    ///
    /// Returns an error if model/page/item/cursor validation fails or bounded
    /// file roll-up evidence and digests cannot be queried or decoded.
    pub async fn pending_file_summaries(
        &self,
        request: PendingSummaryRollupQuery<'_>,
    ) -> Result<Vec<PendingFileSummary>, StorageError> {
        let PendingSummaryRollupQuery {
            project_id,
            model,
            anchor_digest,
            after: after_path,
            limit,
            item_limit,
        } = request;
        validate_model(model)?;
        validate_limit(limit, MAX_FILE_SUMMARY_BATCH)?;
        validate_limit(item_limit, MAX_SUMMARY_ROLLUP_ITEMS)?;
        if let Some(path) = after_path {
            NormalizedPath::parse(path).map_err(|_| StorageError::InvalidInput {
                field: "after_path",
            })?;
        }
        let schema = quoted_schema(&self.schema);
        let statement =
            include_str!("sql/artifacts_pending_file_summaries.sql").replace("{schema}", &schema);
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "pending-file-summaries",
                },
                |statement| {
                    statement
                        .bind(model)
                        .bind(after_path)
                        .bind(i64::from(limit))
                        .bind(i64::from(item_limit))
                        .bind(SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS)
                        .bind(anchor_digest.as_str())
                },
            )
            .await?;
        rows.iter()
            .map(|row| decode_pending_file_summary(row, anchor_digest))
            .collect()
    }

    /// Pull immediate source directories whose bounded symbol roll-up has no
    /// exact model, anchor, generation, and evidence-compatible paragraph.
    /// # Errors
    ///
    /// Returns an error if model/page/item/cursor validation fails or bounded
    /// immediate-directory roll-up evidence cannot be queried or decoded.
    pub async fn pending_module_summaries(
        &self,
        request: PendingSummaryRollupQuery<'_>,
    ) -> Result<Vec<PendingModuleSummary>, StorageError> {
        let PendingSummaryRollupQuery {
            project_id,
            model,
            anchor_digest,
            after: after_directory,
            limit,
            item_limit,
        } = request;
        validate_model(model)?;
        validate_limit(limit, MAX_FILE_SUMMARY_BATCH)?;
        validate_limit(item_limit, MAX_MODULE_SUMMARY_ROLLUP_ITEMS)?;
        if let Some(directory) = after_directory {
            NormalizedPath::parse(directory).map_err(|_| StorageError::InvalidInput {
                field: "after_directory",
            })?;
        }
        let schema = quoted_schema(&self.schema);
        let statement =
            include_str!("sql/artifacts_pending_module_summaries.sql").replace("{schema}", &schema);
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "pending-module-summaries",
                },
                |statement| {
                    statement
                        .bind(model)
                        .bind(after_directory)
                        .bind(i64::from(limit))
                        .bind(i64::from(item_limit))
                        .bind(SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS)
                        .bind(anchor_digest.as_str())
                        .bind(i64::try_from(MIN_MODULE_SUMMARY_SYMBOLS).unwrap_or(i64::MAX))
                },
            )
            .await?;
        rows.iter()
            .map(|row| decode_pending_module_summary(row, anchor_digest))
            .collect()
    }

    /// Pull a deterministic batch whose current structural digest has neither
    /// a high-confidence structural role nor a role from this exact model.
    /// # Errors
    ///
    /// Returns an error if model/result bounds are invalid or pending current
    /// role candidates and their bounded source text cannot be decoded.
    pub async fn pending_symbol_roles(
        &self,
        project_id: &ProjectId,
        sweep: RoleSweepModel<'_>,
        limit: u16,
    ) -> Result<Vec<PendingRoleSymbol>, StorageError> {
        validate_limit(limit, MAX_ROLE_BATCH)?;
        let (model, keep_judged) = match sweep {
            RoleSweepModel::Structural(model) => (model, true),
            RoleSweepModel::Judge(model) => (model, false),
        };
        if model.is_empty() || model.len() > 256 || model.contains('\0') {
            return Err(StorageError::InvalidInput { field: "model" });
        }
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH current AS (
                    SELECT current_generation_id AS generation_id
                    FROM {schema}."projects"
                    WHERE project_id = CAST($1 AS uuid)
                )
                SELECT symbols.symbol_id::text,
                       files.normalized_path,
                       files.language,
                       symbols.symbol_kind,
                       symbols.qualified_name,
                       left(symbols.signature, $4),
                       COALESCE(NULLIF(summaries.body, ''), left(documents.natural_text, $5), ''),
                       COALESCE(left(documents.code, $5), ''),
                       symbols.exported
                FROM {schema}."symbols" AS symbols
                JOIN current ON current.generation_id = symbols.generation_id
                JOIN {schema}."files" AS files
                  ON files.project_id = symbols.project_id
                 AND files.generation_id = symbols.generation_id
                 AND files.file_id = symbols.file_id
                LEFT JOIN {schema}."search_documents" AS documents
                  ON documents.project_id = symbols.project_id
                 AND documents.generation_id = symbols.generation_id
                 AND documents.symbol_id = symbols.symbol_id
                 AND documents.document_kind = 'symbol'
                LEFT JOIN {schema}."agent_artifacts" AS summaries
                  ON summaries.project_id = symbols.project_id
                 AND summaries.artifact_kind = 'summary'
                 AND summaries.scope_kind = 'symbol'
                 AND summaries.scope_key = symbols.symbol_id::text
                 AND summaries.source_digest = symbols.structural_digest
                 AND summaries.state = 'complete'
                LEFT JOIN {schema}."agent_artifacts" AS roles
                  ON roles.project_id = symbols.project_id
                 AND roles.artifact_kind = 'role'
                 AND roles.scope_kind = 'symbol'
                 AND roles.scope_key = symbols.symbol_id::text
                 AND roles.source_digest = symbols.structural_digest
                 AND roles.state = 'complete'
                 AND (
                      roles.metadata->>'via' IN ('structural_rule', 'rule')
                      OR (
                          roles.metadata->>'via' IN ('structural_fallback', 'llm', 'jev')
                          AND roles.metadata->>'model' = $2
                      )
                      OR ($6 AND roles.metadata->>'via' IN ('llm', 'jev'))
                 )
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.symbol_kind NOT IN ('file', 'import', 'parameter')
                  AND roles.id IS NULL
                ORDER BY symbols.exported DESC,
                         (symbols.visibility = 'public') DESC,
                         files.normalized_path,
                         symbols.start_line,
                         symbols.symbol_id
                LIMIT $3"#,
        );
        let rows = self
            .artifact_read(
                ArtifactReadRequest {
                    statement,
                    project_id,
                    operation: "pending-symbol-roles",
                },
                |statement| {
                    statement
                        .bind(model)
                        .bind(i64::from(limit))
                        .bind(SUMMARY_SIGNATURE_MAXIMUM_CHARACTERS)
                        .bind(ROLE_EVIDENCE_MAXIMUM_CHARACTERS)
                        .bind(keep_judged)
                },
            )
            .await?;
        rows.iter().map(decode_pending_role).collect()
    }

    /// Save a summary only when its echoed structural digest still matches current source.
    /// # Errors
    ///
    /// Returns an error if summary/model validation fails, the structural
    /// digest is stale, or the exact current-symbol upsert cannot be decoded.
    pub async fn save_symbol_summary(
        &self,
        input: SymbolSummarySaveInput<'_>,
    ) -> Result<AgentArtifactRecord, StorageError> {
        let SymbolSummarySaveInput {
            project_id,
            symbol_id,
            source_digest,
            summary,
            model,
        } = input;
        validate_body(summary, AgentArtifactState::Complete)?;
        validate_model(model)?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH saved AS (
                INSERT INTO {schema}."agent_artifacts" (
                    project_id, artifact_kind, scope_kind, scope_key, body, metadata,
                    generation_id, source_digest, state
                )
                SELECT symbols.project_id, 'summary', 'symbol', symbols.symbol_id::text,
                       $4, jsonb_build_object('model', $5::text),
                       symbols.generation_id, symbols.structural_digest, 'complete'
                FROM {schema}."symbols" AS symbols
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = symbols.project_id
                 AND projects.current_generation_id = symbols.generation_id
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.symbol_id = CAST($2 AS uuid)
                  AND symbols.structural_digest = $3
                ON CONFLICT (project_id, artifact_kind, scope_kind, scope_key)
                WHERE artifact_kind IN ('role', 'summary') AND state <> 'archived'
                DO UPDATE SET body = EXCLUDED.body,
                              metadata = EXCLUDED.metadata,
                              generation_id = EXCLUDED.generation_id,
                              source_digest = EXCLUDED.source_digest,
                              state = EXCLUDED.state,
                              updated_at = clock_timestamp()
                RETURNING id, artifact_id::text, artifact_kind, scope_kind, scope_key,
                          body, metadata::text, generation_id::text, source_digest,
                          state, created_at::text, updated_at::text,
                          project_id AS saved_project_id,
                          generation_id AS saved_generation_id
                ), cleared AS (
                    DELETE FROM {schema}."summary_priority_queue" AS priority
                    USING saved
                    WHERE priority.project_id = saved.saved_project_id
                      AND priority.generation_id = saved.saved_generation_id
                      AND priority.symbol_id = CAST(saved.scope_key AS uuid)
                    RETURNING priority.symbol_id
                ), invalidated AS (
                    DELETE FROM {schema}."document_embeddings" AS embeddings
                    USING {schema}."search_documents" AS documents, saved
                    WHERE documents.project_id = saved.saved_project_id
                      AND documents.generation_id = saved.saved_generation_id
                      AND documents.symbol_id = CAST(saved.scope_key AS uuid)
                      AND embeddings.project_id = documents.project_id
                      AND embeddings.generation_id = documents.generation_id
                      AND embeddings.document_id = documents.document_id
                    RETURNING embeddings.document_id
                )
                SELECT id, artifact_id, artifact_kind, scope_kind, scope_key,
                       body, metadata, generation_id, source_digest,
                       state, created_at, updated_at,
                       (SELECT COUNT(*) FROM cleared),
                       (SELECT COUNT(*) FROM invalidated)
                FROM saved"#,
        );
        let row = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(symbol_id.as_str())
            .bind(source_digest.as_str())
            .bind(summary)
            .bind(model)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("save-symbol-summary"))?
            .ok_or(StorageError::CurrentGenerationChanged)?;
        decode_artifact(&row)
    }

    /// Publish a deterministic structural fallback without replacing a
    /// digest-compatible summary produced by an LLM or agent. `None` means
    /// either the source generation changed or a higher-quality writer won.
    /// # Errors
    ///
    /// Returns an error if the summary body is invalid or PostgreSQL cannot
    /// apply the current-digest, higher-quality-writer-preserving upsert.
    pub async fn save_structural_symbol_summary(
        &self,
        input: StructuralSymbolSummarySaveInput<'_>,
    ) -> Result<Option<AgentArtifactRecord>, StorageError> {
        let StructuralSymbolSummarySaveInput {
            project_id,
            symbol_id,
            source_digest,
            summary,
        } = input;
        validate_body(summary, AgentArtifactState::Complete)?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH saved AS (
                INSERT INTO {schema}."agent_artifacts" AS existing (
                    project_id, artifact_kind, scope_kind, scope_key, body, metadata,
                    generation_id, source_digest, state
                )
                SELECT symbols.project_id, 'summary', 'symbol', symbols.symbol_id::text,
                       $4, jsonb_build_object(
                           'model', 'structural:v2',
                           'generationMode', 'structural_rule'
                       ), symbols.generation_id, symbols.structural_digest, 'complete'
                FROM {schema}."symbols" AS symbols
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = symbols.project_id
                 AND projects.current_generation_id = symbols.generation_id
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.symbol_id = CAST($2 AS uuid)
                  AND symbols.structural_digest = $3
                ON CONFLICT (project_id, artifact_kind, scope_kind, scope_key)
                WHERE artifact_kind IN ('role', 'summary') AND state <> 'archived'
                DO UPDATE SET body = EXCLUDED.body,
                              metadata = EXCLUDED.metadata,
                              generation_id = EXCLUDED.generation_id,
                              source_digest = EXCLUDED.source_digest,
                              state = EXCLUDED.state,
                              updated_at = clock_timestamp()
                WHERE existing.source_digest IS DISTINCT FROM EXCLUDED.source_digest
                   OR COALESCE(existing.metadata ->> 'model', '') LIKE 'structural:%'
                RETURNING id, artifact_id::text, artifact_kind, scope_kind, scope_key,
                          body, metadata::text, generation_id::text, source_digest,
                          state, created_at::text, updated_at::text,
                          project_id AS saved_project_id,
                          generation_id AS saved_generation_id
                ), invalidated AS (
                    DELETE FROM {schema}."document_embeddings" AS embeddings
                    USING {schema}."search_documents" AS documents, saved
                    WHERE documents.project_id = saved.saved_project_id
                      AND documents.generation_id = saved.saved_generation_id
                      AND documents.symbol_id = CAST(saved.scope_key AS uuid)
                      AND embeddings.project_id = documents.project_id
                      AND embeddings.generation_id = documents.generation_id
                      AND embeddings.document_id = documents.document_id
                    RETURNING embeddings.document_id
                )
                SELECT id, artifact_id, artifact_kind, scope_kind, scope_key,
                       body, metadata, generation_id, source_digest,
                       state, created_at, updated_at,
                       (SELECT COUNT(*) FROM invalidated)
                FROM saved"#,
        );
        let row = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(symbol_id.as_str())
            .bind(source_digest.as_str())
            .bind(summary)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("save-structural-symbol-summary"))?;
        row.as_ref().map(decode_artifact).transpose()
    }

    /// Publish one transparent semantic-neighbor fallback under exact source,
    /// target, generation, kind, and summary-body fences. A current summary
    /// from any producer wins the race and yields `None`.
    /// # Errors
    ///
    /// Returns an error if neighbor/model/body fields are invalid or exact
    /// source/target/generation/digest/summary fences cannot be evaluated.
    pub async fn save_neighbor_symbol_summary(
        &self,
        project_id: &ProjectId,
        request: NeighborSummarySaveRequest<'_>,
    ) -> Result<Option<AgentArtifactRecord>, StorageError> {
        let model = format!("neighbor:v2:{}", request.embedding_model_id.as_str());
        validate_model(&model)?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH saved AS (
                INSERT INTO {schema}."agent_artifacts" AS existing (
                    project_id, artifact_kind, scope_kind, scope_key, body, metadata,
                    generation_id, source_digest, state
                )
                SELECT targets.project_id, 'summary', 'symbol', targets.symbol_id::text,
                       $6, jsonb_build_object(
                           'model', $7::text,
                           'generationMode', 'semantic_neighbor',
                           'sourceSymbolId', sources.symbol_id::text,
                           'sourceModel', COALESCE(
                               NULLIF(source_summaries.metadata ->> 'model', ''), 'agent'
                           ),
                           'embeddingModelId', $8::text,
                           'similarity', $9::float8
                       ), targets.generation_id, targets.structural_digest, 'complete'
                FROM {schema}."symbols" AS targets
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = targets.project_id
                 AND projects.current_generation_id = targets.generation_id
                JOIN {schema}."symbols" AS sources
                  ON sources.project_id = targets.project_id
                 AND sources.generation_id = targets.generation_id
                 AND sources.symbol_id = CAST($4 AS uuid)
                 AND sources.symbol_kind = targets.symbol_kind
                JOIN {schema}."agent_artifacts" AS source_summaries
                  ON source_summaries.project_id = sources.project_id
                 AND source_summaries.artifact_kind = 'summary'
                 AND source_summaries.scope_kind = 'symbol'
                 AND source_summaries.scope_key = sources.symbol_id::text
                 AND source_summaries.state = 'complete'
                 AND source_summaries.source_digest = sources.structural_digest
                 AND source_summaries.body = $5
                 AND COALESCE(source_summaries.metadata ->> 'model', '') NOT LIKE 'neighbor:%'
                WHERE targets.project_id = CAST($1 AS uuid)
                  AND targets.symbol_id = CAST($2 AS uuid)
                  AND targets.structural_digest = $3
                  AND targets.symbol_id <> sources.symbol_id
                ON CONFLICT (project_id, artifact_kind, scope_kind, scope_key)
                WHERE artifact_kind IN ('role', 'summary') AND state <> 'archived'
                DO UPDATE SET body = EXCLUDED.body,
                              metadata = EXCLUDED.metadata,
                              generation_id = EXCLUDED.generation_id,
                              source_digest = EXCLUDED.source_digest,
                              state = EXCLUDED.state,
                              updated_at = clock_timestamp()
                WHERE existing.source_digest IS DISTINCT FROM EXCLUDED.source_digest
                RETURNING id, artifact_id::text, artifact_kind, scope_kind, scope_key,
                          body, metadata::text, generation_id::text, source_digest,
                          state, created_at::text, updated_at::text,
                          project_id AS saved_project_id,
                          generation_id AS saved_generation_id
                ), cleared AS (
                    DELETE FROM {schema}."summary_priority_queue" AS priority
                    USING saved
                    WHERE priority.project_id = saved.saved_project_id
                      AND priority.generation_id = saved.saved_generation_id
                      AND priority.symbol_id = CAST(saved.scope_key AS uuid)
                    RETURNING priority.symbol_id
                )
                SELECT id, artifact_id, artifact_kind, scope_kind, scope_key,
                       body, metadata, generation_id, source_digest,
                       state, created_at, updated_at,
                       (SELECT COUNT(*) FROM cleared)
                FROM saved"#,
        );
        let row = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(request.symbol_id.as_str())
            .bind(request.source_digest.as_str())
            .bind(request.neighbor_symbol_id.as_str())
            .bind(request.neighbor_summary)
            .bind(request.summary)
            .bind(&model)
            .bind(request.embedding_model_id.as_str())
            .bind(request.similarity)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("save-neighbor-symbol-summary"))?;
        row.as_ref().map(decode_artifact).transpose()
    }

    /// Save a file roll-up only if its exact bounded source evidence is still current.
    /// # Errors
    ///
    /// Returns an error if current generation/evidence differs from the echoed
    /// file roll-up contract or the transactional summary upsert cannot commit.
    pub async fn save_file_summary(
        &self,
        project_id: &ProjectId,
        request: FileSummarySaveRequest<'_>,
    ) -> Result<Option<AgentArtifactRecord>, StorageError> {
        let schema = quoted_schema(&self.schema);
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("save-file-summary"))?;
        set_local_statement_timeout(&mut transaction, ARTIFACT_TIMEOUT)
            .await
            .map_err(|()| database_error("save-file-summary"))?;
        let lock = format!(
            r#"SELECT current_generation_id::text
                FROM {schema}."projects"
                WHERE project_id = CAST($1 AS uuid)
                FOR SHARE"#,
        );
        let generation_id = query(AssertSqlSafe(lock))
            .bind(project_id.as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| database_error("save-file-summary"))?
            .and_then(|row| row.try_get::<Option<String>, _>(0).ok().flatten())
            .ok_or(StorageError::CurrentGenerationChanged)?;
        let evidence = file_summary_evidence_statement(&schema);
        let rows = query(AssertSqlSafe(evidence))
            .bind(project_id.as_str())
            .bind(&generation_id)
            .bind(request.path.as_str())
            .bind(i64::from(request.fields.item_limit))
            .bind(SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS)
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("save-file-summary"))?;
        let pending = pending_file_summary_from_evidence_rows(&rows, request.fields.anchor_digest)?
            .ok_or(StorageError::CurrentGenerationChanged)?;
        if pending.content_hash() != request.fields.expected_digest.as_str() {
            return Err(StorageError::CurrentGenerationChanged);
        }
        let metadata = file_summary_metadata(request.fields, &pending)?;
        let upsert = scoped_summary_upsert_statement(
            &schema,
            request.fields.generation_mode == "structural_rule",
        );
        let row = query(AssertSqlSafe(upsert))
            .bind(project_id.as_str())
            .bind(AgentArtifactScope::File.as_str())
            .bind(request.path.as_str())
            .bind(request.fields.summary)
            .bind(metadata)
            .bind(&generation_id)
            .bind(request.fields.expected_digest.as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| database_error("save-file-summary"))?;
        let Some(row) = row else {
            transaction
                .commit()
                .await
                .map_err(|_| database_error("save-file-summary"))?;
            return Ok(None);
        };
        let artifact = decode_artifact(&row)?;
        let invalidate = format!(
            r#"DELETE FROM {schema}."document_embeddings" AS embeddings
                USING {schema}."search_documents" AS documents,
                      {schema}."symbols" AS symbols
                WHERE documents.project_id = CAST($1 AS uuid)
                  AND documents.generation_id = CAST($2 AS uuid)
                  AND documents.path = $3
                  AND symbols.project_id = documents.project_id
                  AND symbols.generation_id = documents.generation_id
                  AND symbols.symbol_id = documents.symbol_id
                  AND symbols.symbol_kind = 'file'
                  AND embeddings.project_id = documents.project_id
                  AND embeddings.generation_id = documents.generation_id
                  AND embeddings.document_id = documents.document_id"#,
        );
        query(AssertSqlSafe(invalidate))
            .bind(project_id.as_str())
            .bind(&generation_id)
            .bind(request.path.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("save-file-summary"))?;
        transaction
            .commit()
            .await
            .map_err(|_| database_error("save-file-summary"))?;
        Ok(Some(artifact))
    }

    /// Save a module roll-up only if its exact immediate-directory evidence is current.
    /// # Errors
    ///
    /// Returns an error if current generation/evidence differs from the echoed
    /// immediate-directory contract or the summary upsert cannot commit.
    pub async fn save_module_summary(
        &self,
        project_id: &ProjectId,
        request: ModuleSummarySaveRequest<'_>,
    ) -> Result<Option<AgentArtifactRecord>, StorageError> {
        let schema = quoted_schema(&self.schema);
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("save-module-summary"))?;
        set_local_statement_timeout(&mut transaction, ARTIFACT_TIMEOUT)
            .await
            .map_err(|()| database_error("save-module-summary"))?;
        let lock = format!(
            r#"SELECT current_generation_id::text
                FROM {schema}."projects"
                WHERE project_id = CAST($1 AS uuid)
                FOR SHARE"#,
        );
        let generation_id = query(AssertSqlSafe(lock))
            .bind(project_id.as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| database_error("save-module-summary"))?
            .and_then(|row| row.try_get::<Option<String>, _>(0).ok().flatten())
            .ok_or(StorageError::CurrentGenerationChanged)?;
        let evidence = module_summary_evidence_statement(&schema);
        let rows = query(AssertSqlSafe(evidence))
            .bind(project_id.as_str())
            .bind(&generation_id)
            .bind(request.directory.as_str())
            .bind(i64::from(request.fields.item_limit))
            .bind(SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS)
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| database_error("save-module-summary"))?;
        let pending =
            pending_module_summary_from_evidence_rows(&rows, request.fields.anchor_digest)?
                .filter(|pending| pending.summarized_symbols() >= MIN_MODULE_SUMMARY_SYMBOLS)
                .ok_or(StorageError::CurrentGenerationChanged)?;
        if pending.content_hash() != request.fields.expected_digest.as_str() {
            return Err(StorageError::CurrentGenerationChanged);
        }
        let metadata = serde_json::to_string(&serde_json::json!({
            "model": request.fields.model,
            "anchorDigest": request.fields.anchor_digest.as_str(),
            "summarizedSymbols": pending.summarized_symbols(),
            "toolExportConstants": pending.tool_export_constants(),
            "generationMode": request.fields.generation_mode,
            "evidenceItems": pending.items().len(),
            "itemsTruncated": pending.items_truncated(),
            "rollupDigest": request.fields.expected_digest.as_str(),
            "protocol": "symbol_to_module_v2",
        }))
        .map_err(|_| StorageError::InvalidInput {
            field: "artifact_metadata",
        })?;
        let upsert = scoped_summary_upsert_statement(
            &schema,
            request.fields.generation_mode == "structural_rule",
        );
        let row = query(AssertSqlSafe(upsert))
            .bind(project_id.as_str())
            .bind(AgentArtifactScope::Module.as_str())
            .bind(request.directory.as_str())
            .bind(request.fields.summary)
            .bind(metadata)
            .bind(&generation_id)
            .bind(request.fields.expected_digest.as_str())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| database_error("save-module-summary"))?;
        let Some(row) = row else {
            transaction
                .commit()
                .await
                .map_err(|_| database_error("save-module-summary"))?;
            return Ok(None);
        };
        let artifact = decode_artifact(&row)?;
        transaction
            .commit()
            .await
            .map_err(|_| database_error("save-module-summary"))?;
        Ok(Some(artifact))
    }

    /// Save one role classification only when the symbol is still current.
    ///
    /// The structural digest is persisted with the role so publication can
    /// carry the classification forward without another model call when the
    /// declaration is unchanged. A changed declaration intentionally leaves
    /// the old artifact generation-bound and pending reclassification.
    /// # Errors
    ///
    /// Returns an error if role/metadata validation fails, the symbol is not in
    /// the current generation, or the digest-bound role upsert cannot be decoded.
    pub async fn save_symbol_role(
        &self,
        input: SymbolRoleSaveInput<'_>,
    ) -> Result<AgentArtifactRecord, StorageError> {
        let SymbolRoleSaveInput {
            project_id,
            symbol_id,
            role,
            metadata,
        } = input;
        validate_body(role, AgentArtifactState::Complete)?;
        validate_metadata(&metadata)?;
        let metadata =
            serde_json::to_string(&metadata).map_err(|_| StorageError::InvalidInput {
                field: "artifact_metadata",
            })?;
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"INSERT INTO {schema}."agent_artifacts" (
                    project_id, artifact_kind, scope_kind, scope_key, body, metadata,
                    generation_id, source_digest, state
                )
                SELECT symbols.project_id, 'role', 'symbol', symbols.symbol_id::text,
                       $3, CAST($4 AS jsonb), symbols.generation_id,
                       symbols.structural_digest, 'complete'
                FROM {schema}."symbols" AS symbols
                JOIN {schema}."projects" AS projects
                  ON projects.project_id = symbols.project_id
                 AND projects.current_generation_id = symbols.generation_id
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.symbol_id = CAST($2 AS uuid)
                ON CONFLICT (project_id, artifact_kind, scope_kind, scope_key)
                WHERE artifact_kind IN ('role', 'summary') AND state <> 'archived'
                DO UPDATE SET body = EXCLUDED.body,
                              metadata = EXCLUDED.metadata,
                              generation_id = EXCLUDED.generation_id,
                              source_digest = EXCLUDED.source_digest,
                              state = EXCLUDED.state,
                              updated_at = clock_timestamp()
                RETURNING id, artifact_id::text, artifact_kind, scope_kind, scope_key,
                          body, metadata::text, generation_id::text, source_digest,
                          state, created_at::text, updated_at::text"#,
        );
        let row = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .bind(symbol_id.as_str())
            .bind(role)
            .bind(metadata)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| database_error("save-symbol-role"))?
            .ok_or(StorageError::CurrentGenerationChanged)?;
        decode_artifact(&row)
    }

    async fn artifact_read<'query, Bind>(
        &self,
        request: ArtifactReadRequest<'_>,
        bind: Bind,
    ) -> Result<Vec<sqlx_postgres::PgRow>, StorageError>
    where
        Bind: FnOnce(
            sqlx_core::query::Query<'query, sqlx_postgres::Postgres, sqlx_postgres::PgArguments>,
        ) -> sqlx_core::query::Query<
            'query,
            sqlx_postgres::Postgres,
            sqlx_postgres::PgArguments,
        >,
    {
        let ArtifactReadRequest {
            statement,
            project_id,
            operation,
        } = request;
        read_project_rows(
            self,
            ProjectReadRequest {
                statement,
                project_id,
                operation,
                statement_timeout: ARTIFACT_TIMEOUT,
            },
            bind,
        )
        .await
    }
}

fn file_summary_metadata(
    fields: SummarySaveFields<'_>,
    pending: &PendingFileSummary,
) -> Result<String, StorageError> {
    serde_json::to_string(&serde_json::json!({
        "model": fields.model,
        "anchorDigest": fields.anchor_digest.as_str(),
        "fileContentHash": pending.file_content_hash(),
        "summarizedSymbols": pending.summarized_symbols(),
        "evidenceItems": pending.items().len(),
        "itemsTruncated": pending.items_truncated(),
        "rollupDigest": fields.expected_digest.as_str(),
        "generationMode": fields.generation_mode,
        "protocol": "symbol_to_file_v2",
    }))
    .map_err(|_| StorageError::InvalidInput {
        field: "artifact_metadata",
    })
}

fn file_summary_evidence_statement(schema: &str) -> String {
    format!(
        r#"SELECT files.generation_id::text,
                   files.normalized_path,
                   files.content_hash,
                   symbols.symbol_id::text,
                   symbols.qualified_name,
                   symbols.symbol_kind,
                   left(summaries.body, $5),
                   COUNT(*) OVER ()::bigint
            FROM {schema}."files" AS files
            JOIN {schema}."symbols" AS symbols
              ON symbols.project_id = files.project_id
             AND symbols.generation_id = files.generation_id
             AND symbols.file_id = files.file_id
            JOIN {schema}."agent_artifacts" AS summaries
              ON summaries.project_id = symbols.project_id
             AND summaries.artifact_kind = 'summary'
             AND summaries.scope_kind = 'symbol'
             AND summaries.scope_key = symbols.symbol_id::text
             AND summaries.source_digest = symbols.structural_digest
             AND summaries.state = 'complete'
            WHERE files.project_id = CAST($1 AS uuid)
              AND files.generation_id = CAST($2 AS uuid)
              AND files.normalized_path = $3
            ORDER BY symbols.exported DESC,
                     (symbols.visibility = 'public') DESC,
                     symbols.pagerank DESC NULLS LAST,
                     symbols.start_line,
                     symbols.symbol_id
            LIMIT $4"#,
    )
}

fn module_summary_evidence_statement(schema: &str) -> String {
    format!(
        r#"SELECT files.generation_id::text,
                   files.normalized_path,
                   symbols.symbol_id::text,
                   symbols.qualified_name,
                   symbols.symbol_kind,
                   left(summaries.body, $5),
                   COUNT(*) OVER ()::bigint,
                   (
                       SELECT COUNT(*)::bigint
                       FROM {schema}."files" AS tool_files
                       JOIN {schema}."symbols" AS tool_symbols
                         ON tool_symbols.project_id = tool_files.project_id
                        AND tool_symbols.generation_id = tool_files.generation_id
                        AND tool_symbols.file_id = tool_files.file_id
                       WHERE tool_files.project_id = CAST($1 AS uuid)
                         AND tool_files.generation_id = CAST($2 AS uuid)
                         AND LEFT(tool_files.normalized_path, char_length($3) + 1) = $3 || '/'
                         AND POSITION('/' IN SUBSTRING(
                               tool_files.normalized_path FROM char_length($3) + 2
                             )) = 0
                         AND tool_symbols.symbol_kind = 'constant'
                         AND tool_symbols.qualified_name ~ '_TOOL$'
                   )
            FROM {schema}."files" AS files
            JOIN {schema}."symbols" AS symbols
              ON symbols.project_id = files.project_id
             AND symbols.generation_id = files.generation_id
             AND symbols.file_id = files.file_id
            JOIN {schema}."agent_artifacts" AS summaries
              ON summaries.project_id = symbols.project_id
             AND summaries.artifact_kind = 'summary'
             AND summaries.scope_kind = 'symbol'
             AND summaries.scope_key = symbols.symbol_id::text
             AND summaries.source_digest = symbols.structural_digest
             AND summaries.state = 'complete'
            WHERE files.project_id = CAST($1 AS uuid)
              AND files.generation_id = CAST($2 AS uuid)
              AND LEFT(files.normalized_path, char_length($3) + 1) = $3 || '/'
              AND POSITION('/' IN SUBSTRING(
                    files.normalized_path FROM char_length($3) + 2
                  )) = 0
            ORDER BY symbols.exported DESC,
                     (symbols.visibility = 'public') DESC,
                     symbols.pagerank DESC NULLS LAST,
                     files.normalized_path,
                     symbols.start_line,
                     symbols.symbol_id
            LIMIT $4"#,
    )
}

fn scoped_summary_upsert_statement(schema: &str, preserve_higher_quality: bool) -> String {
    let guard = if preserve_higher_quality {
        "WHERE existing.generation_id IS DISTINCT FROM EXCLUDED.generation_id\n               OR existing.source_digest IS DISTINCT FROM EXCLUDED.source_digest\n               OR COALESCE(existing.metadata ->> 'generationMode', '') = 'structural_rule'\n               OR COALESCE(existing.metadata ->> 'model', '') LIKE 'structural:%'"
    } else {
        ""
    };
    format!(
        r#"INSERT INTO {schema}."agent_artifacts" AS existing (
                project_id, artifact_kind, scope_kind, scope_key, body, metadata,
                generation_id, source_digest, state
            ) VALUES (
                CAST($1 AS uuid), 'summary', $2, $3, $4, CAST($5 AS jsonb),
                CAST($6 AS uuid), $7, 'complete'
            )
            ON CONFLICT (project_id, artifact_kind, scope_kind, scope_key)
            WHERE artifact_kind IN ('role', 'summary') AND state <> 'archived'
            DO UPDATE SET body = EXCLUDED.body,
                          metadata = EXCLUDED.metadata,
                          generation_id = EXCLUDED.generation_id,
                          source_digest = EXCLUDED.source_digest,
                          state = EXCLUDED.state,
                          updated_at = clock_timestamp()
            {guard}
            RETURNING id, artifact_id::text, artifact_kind, scope_kind, scope_key,
                      body, metadata::text, generation_id::text, source_digest,
                      state, created_at::text, updated_at::text"#,
    )
}

fn pending_file_summary_from_evidence_rows(
    rows: &[sqlx_postgres::PgRow],
    anchor_digest: &ContentDigest,
) -> Result<Option<PendingFileSummary>, StorageError> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let generation_id = text(first, 0)?;
    let path = text(first, 1)?;
    let file_content_hash = text(first, 2)?;
    let summarized_symbols = nonnegative_u64(first, 7)?;
    let items = rows
        .iter()
        .map(|row| {
            if text(row, 0)? != generation_id
                || text(row, 1)? != path
                || text(row, 2)? != file_content_hash
                || nonnegative_u64(row, 7)? != summarized_symbols
            {
                return Err(StorageError::CorruptStoredValue {
                    field: "file_summary_evidence",
                });
            }
            Ok(SummaryRollupItem {
                symbol_id: text(row, 3)?,
                qualified_name: text(row, 4)?,
                symbol_kind: text(row, 5)?,
                summary: text(row, 6)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let content_hash = summary_rollup_digest(SummaryRollupDigestInput {
        domain: "cartograph:file-summary:v2",
        scope_key: &path,
        source_hash: &file_content_hash,
        total_items: summarized_symbols,
        items: &items,
        anchor_digest,
    });
    Ok(Some(PendingFileSummary {
        generation_id,
        path,
        file_content_hash,
        content_hash: content_hash.as_str().to_owned(),
        summarized_symbols,
        items_truncated: summarized_symbols
            > u64::try_from(items.len()).map_err(|_| StorageError::CorruptStoredValue {
                field: "file_summary_evidence",
            })?,
        items,
    }))
}

fn pending_module_summary_from_evidence_rows(
    rows: &[sqlx_postgres::PgRow],
    anchor_digest: &ContentDigest,
) -> Result<Option<PendingModuleSummary>, StorageError> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let generation_id = text(first, 0)?;
    let first_path = text(first, 1)?;
    let directory =
        immediate_parent_directory(&first_path).ok_or(StorageError::CorruptStoredValue {
            field: "module_summary_evidence",
        })?;
    let summarized_symbols = nonnegative_u64(first, 6)?;
    let tool_export_constants = nonnegative_u64(first, 7)?;
    let items = rows
        .iter()
        .map(|row| {
            let path = text(row, 1)?;
            if text(row, 0)? != generation_id
                || immediate_parent_directory(&path).as_deref() != Some(directory.as_str())
                || nonnegative_u64(row, 6)? != summarized_symbols
                || nonnegative_u64(row, 7)? != tool_export_constants
            {
                return Err(StorageError::CorruptStoredValue {
                    field: "module_summary_evidence",
                });
            }
            Ok(ModuleSummaryRollupItem {
                symbol_id: text(row, 2)?,
                path,
                qualified_name: text(row, 3)?,
                symbol_kind: text(row, 4)?,
                summary: text(row, 5)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let content_hash = module_summary_rollup_digest(ModuleSummaryRollupDigestInput {
        directory: &directory,
        total_items: summarized_symbols,
        tool_export_constants,
        items: &items,
        anchor_digest,
    });
    Ok(Some(PendingModuleSummary {
        generation_id,
        directory,
        content_hash: content_hash.as_str().to_owned(),
        summarized_symbols,
        tool_export_constants,
        items_truncated: summarized_symbols
            > u64::try_from(items.len()).map_err(|_| StorageError::CorruptStoredValue {
                field: "module_summary_evidence",
            })?,
        items,
    }))
}

const FILE_SUMMARY_SYMBOL_COUNT_COLUMN: usize = 3;
const FILE_SUMMARY_ITEMS_COLUMN: usize = 4;
const FILE_SUMMARY_TRUNCATED_COLUMN: usize = 5;
const ROLE_SYMBOL_KIND_COLUMN: usize = 3;
const ROLE_QUALIFIED_NAME_COLUMN: usize = 4;
const ROLE_SIGNATURE_COLUMN: usize = 5;
const ROLE_DESCRIPTION_COLUMN: usize = 6;
const ROLE_CODE_COLUMN: usize = 7;
const ROLE_EXPORTED_COLUMN: usize = 8;

fn decode_pending_file_summary(
    row: &sqlx_postgres::PgRow,
    anchor_digest: &ContentDigest,
) -> Result<PendingFileSummary, StorageError> {
    let generation_id = text(row, 0)?;
    let path = text(row, 1)?;
    let file_content_hash = text(row, 2)?;
    let summarized_symbols = nonnegative_u64(row, FILE_SUMMARY_SYMBOL_COUNT_COLUMN)?;
    let items =
        serde_json::from_str::<Vec<SummaryRollupItem>>(&text(row, FILE_SUMMARY_ITEMS_COLUMN)?)
            .map_err(|_| StorageError::CorruptStoredValue {
                field: "file_summary_evidence",
            })?;
    if items.is_empty()
        || items.len() > usize::from(MAX_SUMMARY_ROLLUP_ITEMS)
        || items.iter().any(invalid_summary_rollup_item)
    {
        return Err(StorageError::CorruptStoredValue {
            field: "file_summary_evidence",
        });
    }
    let items_truncated = row
        .try_get::<bool, _>(FILE_SUMMARY_TRUNCATED_COLUMN)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: "file_summary_evidence",
        })?;
    let content_hash = summary_rollup_digest(SummaryRollupDigestInput {
        domain: "cartograph:file-summary:v2",
        scope_key: &path,
        source_hash: &file_content_hash,
        total_items: summarized_symbols,
        items: &items,
        anchor_digest,
    });
    Ok(PendingFileSummary {
        generation_id,
        path,
        file_content_hash,
        content_hash: content_hash.as_str().to_owned(),
        summarized_symbols,
        items,
        items_truncated,
    })
}

fn invalid_summary_rollup_item(item: &SummaryRollupItem) -> bool {
    item.symbol_id.is_empty()
        || item.qualified_name.is_empty()
        || item.symbol_kind.is_empty()
        || item.summary.is_empty()
        || item.summary.len()
            > usize::try_from(SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS).unwrap_or(usize::MAX)
}

fn decode_pending_module_summary(
    row: &sqlx_postgres::PgRow,
    anchor_digest: &ContentDigest,
) -> Result<PendingModuleSummary, StorageError> {
    let generation_id = text(row, "generation_id")?;
    let directory = text(row, "directory")?;
    NormalizedPath::parse(&directory).map_err(|_| StorageError::CorruptStoredValue {
        field: "module_summary_evidence",
    })?;
    let summarized_symbols = nonnegative_u64(row, "summarized_symbols")?;
    let tool_export_constants = nonnegative_u64(row, "tool_export_constants")?;
    let items = decode_module_summary_items(row, &directory)?;
    if summarized_symbols < MIN_MODULE_SUMMARY_SYMBOLS
        || items.is_empty()
        || items.len() > usize::from(MAX_MODULE_SUMMARY_ROLLUP_ITEMS)
    {
        return Err(StorageError::CorruptStoredValue {
            field: "module_summary_evidence",
        });
    }
    let items_truncated = row.try_get::<bool, _>("items_truncated").map_err(|_| {
        StorageError::CorruptStoredValue {
            field: "module_summary_evidence",
        }
    })?;
    let content_hash = module_summary_rollup_digest(ModuleSummaryRollupDigestInput {
        directory: &directory,
        total_items: summarized_symbols,
        tool_export_constants,
        items: &items,
        anchor_digest,
    });
    Ok(PendingModuleSummary {
        generation_id,
        directory,
        content_hash: content_hash.as_str().to_owned(),
        summarized_symbols,
        tool_export_constants,
        items,
        items_truncated,
    })
}

fn decode_module_summary_items(
    row: &sqlx_postgres::PgRow,
    directory: &str,
) -> Result<Vec<ModuleSummaryRollupItem>, StorageError> {
    let items = serde_json::from_str::<Vec<ModuleSummaryRollupItem>>(&text(row, "items")?)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: "module_summary_evidence",
        })?;
    if items
        .iter()
        .all(|item| valid_module_summary_item(item, directory))
    {
        Ok(items)
    } else {
        Err(StorageError::CorruptStoredValue {
            field: "module_summary_evidence",
        })
    }
}

fn valid_module_summary_item(item: &ModuleSummaryRollupItem, directory: &str) -> bool {
    if immediate_parent_directory(&item.path).as_deref() != Some(directory) {
        return false;
    }
    if !module_summary_item_has_required_text(item) {
        return false;
    }
    let maximum_characters =
        usize::try_from(SUMMARY_ROLLUP_ITEM_MAXIMUM_CHARACTERS).unwrap_or(usize::MAX);
    item.summary.len() <= maximum_characters
}

fn module_summary_item_has_required_text(item: &ModuleSummaryRollupItem) -> bool {
    !item.symbol_id.is_empty()
        && !item.qualified_name.is_empty()
        && !item.symbol_kind.is_empty()
        && !item.summary.is_empty()
}

fn summary_rollup_digest(input: SummaryRollupDigestInput<'_>) -> ContentDigest {
    let SummaryRollupDigestInput {
        domain,
        scope_key,
        source_hash,
        total_items,
        items,
        anchor_digest,
    } = input;
    let mut hasher = blake3::Hasher::new();
    for field in [domain, scope_key, source_hash, anchor_digest.as_str()] {
        hash_rollup_field(&mut hasher, field.as_bytes());
    }
    hash_rollup_field(&mut hasher, &total_items.to_be_bytes());
    for item in items {
        for field in [
            item.symbol_id.as_str(),
            item.qualified_name.as_str(),
            item.symbol_kind.as_str(),
            item.summary.as_str(),
        ] {
            hash_rollup_field(&mut hasher, field.as_bytes());
        }
    }
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn module_summary_rollup_digest(input: ModuleSummaryRollupDigestInput<'_>) -> ContentDigest {
    let ModuleSummaryRollupDigestInput {
        directory,
        total_items,
        tool_export_constants,
        items,
        anchor_digest,
    } = input;
    let mut hasher = blake3::Hasher::new();
    for field in [
        "cartograph:module-summary:v2",
        directory,
        anchor_digest.as_str(),
    ] {
        hash_rollup_field(&mut hasher, field.as_bytes());
    }
    hash_rollup_field(&mut hasher, &total_items.to_be_bytes());
    hash_rollup_field(&mut hasher, &tool_export_constants.to_be_bytes());
    for item in items {
        for field in [
            item.symbol_id.as_str(),
            item.path.as_str(),
            item.qualified_name.as_str(),
            item.symbol_kind.as_str(),
            item.summary.as_str(),
        ] {
            hash_rollup_field(&mut hasher, field.as_bytes());
        }
    }
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn immediate_parent_directory(path: &str) -> Option<String> {
    let (directory, file) = path.rsplit_once('/')?;
    (!directory.is_empty() && !file.is_empty()).then(|| directory.to_owned())
}

fn hash_rollup_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

fn validate_artifact(artifact: &NewAgentArtifact) -> Result<(), StorageError> {
    validate_scope_key(&artifact.scope_key)?;
    validate_body(&artifact.body, artifact.state)?;
    validate_metadata(&artifact.metadata)
}

fn validate_model(model: &str) -> Result<(), StorageError> {
    if model.is_empty() || model.len() > 256 || model.contains('\0') {
        Err(StorageError::InvalidInput { field: "model" })
    } else {
        Ok(())
    }
}

fn validate_scope_key(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > MAX_ARTIFACT_KEY_BYTES || value.contains('\0') {
        Err(StorageError::InvalidInput {
            field: "artifact_scope_key",
        })
    } else {
        Ok(())
    }
}

fn validate_body(value: &str, state: AgentArtifactState) -> Result<(), StorageError> {
    if value.len() > MAX_ARTIFACT_BODY_BYTES
        || value.contains('\0')
        || (value.is_empty() && state != AgentArtifactState::Pending)
    {
        Err(StorageError::InvalidInput {
            field: "artifact_body",
        })
    } else {
        Ok(())
    }
}

fn validate_metadata(value: &Value) -> Result<(), StorageError> {
    validate_json_object(value, MAX_ARTIFACT_METADATA_BYTES, "artifact_metadata")
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn decode_artifact(row: &sqlx_postgres::PgRow) -> Result<AgentArtifactRecord, StorageError> {
    let id = decode_artifact_id(row)?;
    let kind = decode_artifact_kind(row)?;
    let scope = decode_artifact_scope(row)?;
    let state = decode_artifact_state(row)?;
    let metadata = decode_artifact_metadata(row)?;
    Ok(AgentArtifactRecord {
        id,
        artifact_id: text(row, "artifact_id")?,
        kind,
        scope,
        scope_key: text(row, "scope_key")?,
        body: text(row, "body")?,
        metadata,
        generation_id: optional_text(row, "generation_id")?,
        source_digest: optional_text(row, "source_digest")?,
        state,
        created_at: text(row, "created_at")?,
        updated_at: text(row, "updated_at")?,
    })
}

fn decode_artifact_id(row: &sqlx_postgres::PgRow) -> Result<u64, StorageError> {
    let id = row
        .try_get::<i64, _>("id")
        .ok()
        .and_then(|id| u64::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or(StorageError::CorruptStoredValue {
            field: "artifact_id",
        })?;
    Ok(id)
}

fn decode_artifact_kind(row: &sqlx_postgres::PgRow) -> Result<AgentArtifactKind, StorageError> {
    text(row, "artifact_kind").and_then(|value| {
        AgentArtifactKind::parse(&value).ok_or(StorageError::CorruptStoredValue {
            field: "artifact_kind",
        })
    })
}

fn decode_artifact_scope(row: &sqlx_postgres::PgRow) -> Result<AgentArtifactScope, StorageError> {
    text(row, "scope_kind").and_then(|value| {
        AgentArtifactScope::parse(&value).ok_or(StorageError::CorruptStoredValue {
            field: "artifact_scope",
        })
    })
}

fn decode_artifact_state(row: &sqlx_postgres::PgRow) -> Result<AgentArtifactState, StorageError> {
    text(row, "state").and_then(|value| {
        AgentArtifactState::parse(&value).ok_or(StorageError::CorruptStoredValue {
            field: "artifact_state",
        })
    })
}

fn decode_artifact_metadata(row: &sqlx_postgres::PgRow) -> Result<Value, StorageError> {
    text(row, "metadata").and_then(|value| {
        serde_json::from_str(&value).map_err(|_| StorageError::CorruptStoredValue {
            field: "artifact_metadata",
        })
    })
}

fn decode_pending_summary(
    row: &sqlx_postgres::PgRow,
) -> Result<PendingSummarySymbol, StorageError> {
    Ok(PendingSummarySymbol {
        generation_id: text(row, "generation_id")?,
        symbol_id: text(row, "symbol_id")?,
        path: text(row, "path")?,
        language: text(row, "language")?,
        symbol_kind: text(row, "symbol_kind")?,
        qualified_name: text(row, "qualified_name")?,
        signature: text(row, "signature")?,
        start_line: positive_u32(row, "start_line")?,
        end_line: positive_u32(row, "end_line")?,
        content_hash: text(row, "content_hash")?,
        code: text(row, "code")?,
        code_truncated: row
            .try_get::<bool, _>("code_truncated")
            .map_err(|_| StorageError::CorruptStoredValue { field: "artifact" })?,
        priority: row
            .try_get::<bool, _>("priority")
            .map_err(|_| StorageError::CorruptStoredValue { field: "artifact" })?,
    })
}

fn decode_pending_structural_summary(
    row: &sqlx_postgres::PgRow,
) -> Result<PendingStructuralSummary, StorageError> {
    let edges = serde_json::from_str(&text(row, "edges")?).map_err(|_| {
        StorageError::CorruptStoredValue {
            field: "structural_summary_edges",
        }
    })?;
    Ok(PendingStructuralSummary {
        generation_id: text(row, "generation_id")?,
        symbol_id: text(row, "symbol_id")?,
        path: text(row, "path")?,
        symbol_kind: text(row, "symbol_kind")?,
        name: text(row, "name")?,
        qualified_name: text(row, "qualified_name")?,
        signature: text(row, "signature")?,
        code: text(row, "code")?,
        start_line: positive_u32(row, "start_line")?,
        end_line: positive_u32(row, "end_line")?,
        content_hash: text(row, "content_hash")?,
        declaration_only: row.try_get::<bool, _>("declaration_only").map_err(|_| {
            StorageError::CorruptStoredValue {
                field: "structural_summary",
            }
        })?,
        edges,
    })
}

fn decode_pending_neighbor_summary(
    row: &sqlx_postgres::PgRow,
) -> Result<PendingNeighborSummary, StorageError> {
    Ok(PendingNeighborSummary {
        generation_id: text(row, 0)?,
        symbol_id: text(row, 1)?,
        symbol_kind: text(row, 2)?,
        name: text(row, 3)?,
        content_hash: text(row, 4)?,
    })
}

fn decode_neighbor_summary_source(
    row: &sqlx_postgres::PgRow,
) -> Result<NeighborSummarySource, StorageError> {
    Ok(NeighborSummarySource {
        symbol_id: text(row, 0)?,
        symbol_kind: text(row, 1)?,
        name: text(row, 2)?,
        summary: text(row, 3)?,
        model: text(row, 4)?,
    })
}

fn decode_pending_role(row: &sqlx_postgres::PgRow) -> Result<PendingRoleSymbol, StorageError> {
    Ok(PendingRoleSymbol {
        symbol_id: text(row, 0)?,
        path: text(row, 1)?,
        language: text(row, 2)?,
        symbol_kind: text(row, ROLE_SYMBOL_KIND_COLUMN)?,
        qualified_name: text(row, ROLE_QUALIFIED_NAME_COLUMN)?,
        signature: text(row, ROLE_SIGNATURE_COLUMN)?,
        description: text(row, ROLE_DESCRIPTION_COLUMN)?,
        code: text(row, ROLE_CODE_COLUMN)?,
        exported: row
            .try_get::<bool, _>(ROLE_EXPORTED_COLUMN)
            .map_err(|_| StorageError::CorruptStoredValue { field: "artifact" })?,
    })
}

fn text<Index>(row: &sqlx_postgres::PgRow, index: Index) -> Result<String, StorageError>
where
    Index: ColumnIndex<sqlx_postgres::PgRow>,
{
    row.try_get(index)
        .map_err(|_| StorageError::CorruptStoredValue { field: "artifact" })
}

fn optional_text<Index>(
    row: &sqlx_postgres::PgRow,
    index: Index,
) -> Result<Option<String>, StorageError>
where
    Index: ColumnIndex<sqlx_postgres::PgRow>,
{
    row.try_get(index)
        .map_err(|_| StorageError::CorruptStoredValue { field: "artifact" })
}

fn positive_u32<Index>(row: &sqlx_postgres::PgRow, index: Index) -> Result<u32, StorageError>
where
    Index: ColumnIndex<sqlx_postgres::PgRow>,
{
    row.try_get::<i32, _>(index)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(StorageError::CorruptStoredValue { field: "artifact" })
}

fn nonnegative_u64<Index>(row: &sqlx_postgres::PgRow, index: Index) -> Result<u64, StorageError>
where
    Index: ColumnIndex<sqlx_postgres::PgRow>,
{
    row.try_get::<i64, _>(index)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(StorageError::CorruptStoredValue { field: "artifact" })
}

fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_bounds_and_identifier_validation_fail_closed() {
        assert!(
            NewAgentArtifact::new(
                AgentArtifactKind::Note,
                AgentArtifactScope::Project,
                AgentArtifactContent::new("project", "remember this")
            )
            .is_ok()
        );
        assert!(
            NewAgentArtifact::new(
                AgentArtifactKind::Note,
                AgentArtifactScope::Project,
                AgentArtifactContent::new("", "remember this")
            )
            .is_err()
        );
        assert!(!canonical_uuid("../../not-an-artifact"));
        assert!(canonical_uuid("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));
    }

    #[test]
    fn metadata_must_be_a_bounded_json_object() {
        let artifact = NewAgentArtifact::new(
            AgentArtifactKind::Role,
            AgentArtifactScope::Symbol,
            AgentArtifactContent::new("symbol", "business_logic"),
        )
        .unwrap_or_else(|error| panic!("valid artifact failed: {error}"));
        assert!(
            artifact
                .clone()
                .with_metadata(serde_json::json!({"via": "rule"}))
                .is_ok()
        );
        assert!(
            artifact
                .with_metadata(serde_json::json!(["not", "an", "object"]))
                .is_err()
        );
    }

    #[test]
    fn summary_candidate_policy_is_bounded_and_keeps_the_route_floor() {
        let default = SummaryCandidatePolicy::default();
        assert_eq!(default.minimum_body_lines, 4);
        assert_eq!(default.minimum_body_lines_by_kind.get("route"), Some(&1));
        assert!(SummaryCandidatePolicy::new(2, BTreeMap::from([("method".to_owned(), 1)])).is_ok());
        assert!(
            SummaryCandidatePolicy::new(2, BTreeMap::from([("bad\nkind".to_owned(), 1)])).is_err()
        );
        assert!(
            SummaryCandidatePolicy::new(MAX_SUMMARY_MINIMUM_BODY_LINES + 1, BTreeMap::new())
                .is_err()
        );
    }
}
