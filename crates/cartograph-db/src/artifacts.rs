use std::{collections::BTreeMap, time::Duration};

pub(crate) mod native;

use cartograph_domain::{
    ContentDigest, GenerationId, ModelId, NormalizedPath, ProjectId, SymbolId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
const MAX_ROLE_BATCH: u16 = 320;
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
}

fn valid_summary_kind_override(kind: &str, floor: u32) -> bool {
    !kind.is_empty()
        && kind.len() <= MAX_SUMMARY_KIND_BYTES
        && !kind.chars().any(char::is_control)
        && floor <= MAX_SUMMARY_MINIMUM_BODY_LINES
}

/// Durable agent-memory family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// Stable scope axis for notes, summaries, roles, and sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// Lifecycle for one durable artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl AgentArtifactState {}

/// Validated write payload for a native agent artifact.
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

/// One durable native artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Copy)]
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
    generation_id: Option<&'a GenerationId>,
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
            generation_id: None,
        }
    }

    #[must_use]
    /// Sets the metadata and returns the updated value.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Reject the save if a different generation publishes after candidate selection.
    #[must_use]
    pub const fn in_generation(mut self, generation: &'a GenerationId) -> Self {
        self.generation_id = Some(generation);
        self
    }
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

/// Opaque native scan position for a structural role sweep, pinned to its source and model.
#[derive(Clone)]
pub struct StructuralRoleCursor {
    generation: GenerationId,
    model: String,
    after: crate::rocks::RecordKey,
}

/// One finite symbol-ID-ordered page for rules that use declaration metadata only.
/// An empty page with a continuation means all scanned declarations already have current roles.
pub struct StructuralRolePage {
    /// Immutable source generation used by this page and its continuation.
    pub generation_id: GenerationId,
    /// Candidates with path, name, kind, signature and export metadata; code and description are empty.
    pub symbols: Vec<PendingRoleSymbol>,
    /// Continue even when `symbols` is empty. `None` means the scan is complete.
    pub next: Option<StructuralRoleCursor>,
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
    /// Durable artifact family.
    #[must_use]
    pub const fn kind(&self) -> AgentArtifactKind {
        self.kind
    }
    /// Source scope represented by this artifact.
    #[must_use]
    pub const fn scope(&self) -> AgentArtifactScope {
        self.scope
    }

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

use crate::{StorageError, database::validate_json_object};

use crate::database::validate_bounded_limit as validate_limit;

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
