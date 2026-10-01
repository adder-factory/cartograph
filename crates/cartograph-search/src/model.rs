use std::{collections::BTreeSet, fmt};

use cartograph_db::{
    CurrentEntryPointPage, CurrentFileRecord, CurrentReferenceRecord, CurrentSymbolRecord,
    EntryPointBucket, ReferenceSpanPrecision as DatabaseReferenceSpanPrecision, SearchComponent,
    SemanticStorageError, StorageError,
};
use cartograph_domain::{
    DocumentId, DocumentKind, EdgeKind, FileId, GenerationId, ModelId, NormalizedPath, ProjectId,
    SourceLanguage, SymbolId,
};
use serde::Serialize;
use thiserror::Error;

use crate::hybrid::{
    FusedSearchItem, HybridSearchPacket, LexicalComponent, RetrievalChannel, SearchMode,
    SemanticReadiness,
};
use crate::intent::{ContextGraphDirection, TaskIntent};
use crate::traversal::is_test_path;

/// Maximum UTF-8 bytes accepted for a context/BM25 query.
pub const CONTEXT_QUERY_MAXIMUM_BYTES: usize = 1_024;
/// Maximum UTF-8 bytes accepted for exact name/reference anchors.
pub const CONTEXT_ANCHOR_MAXIMUM_BYTES: usize = 4_096;
const MAX_ANCHORS: usize = 16;
const MAX_ROOTS: usize = 32;
const MAX_DEPTH: u8 = 50;
const MAX_GRAPH_NODES: u16 = 500;
const MAX_CANDIDATES: u16 = 100;
const MAX_EXACT_RESULTS: u16 = 500;
const MAX_FILE_INVENTORY_RESULTS: u16 = 500;
const MAX_ENTRY_POINT_RESULTS: u16 = 200;
const MAX_SOURCE_RANGE_RESULTS: u16 = 200;
const MAX_SOURCE_RANGE_LINES: u32 = 100_000;
const MAX_PACKET_EVIDENCE: u16 = 100;
const MAX_AFFECTED_TESTS: u16 = 500;
const MAX_REVIEW_CHANGED_PATHS: usize = 512;
/// Maximum changed paths considered by the live working-tree overlay.
pub const WORKING_TREE_OVERLAY_MAXIMUM_FILES: u16 = 64;
/// Maximum live source bytes considered across one overlay request.
pub const WORKING_TREE_OVERLAY_MAXIMUM_SOURCE_BYTES: usize = 4 * 1_048_576;
/// Maximum UTF-8 bytes returned by one overlay excerpt.
pub const WORKING_TREE_OVERLAY_MAXIMUM_EXCERPT_BYTES: usize = 8 * 1_024;
/// Maximum matched live files returned in one context packet.
pub const WORKING_TREE_OVERLAY_MAXIMUM_RESULTS: usize = 12;
const WORKING_TREE_OVERLAY_MAXIMUM_TERMS: usize = 16;
const WORKING_TREE_OVERLAY_MAXIMUM_TERM_BYTES: usize = 128;
const MAX_REVIEW_ROOTS: u16 = 32;
const DEFAULT_TRAVERSAL_NODES: u16 = 100;
const DEFAULT_CONTEXT_CANDIDATES: u16 = 20;
const DEFAULT_CONTEXT_EXACT_RESULTS: u16 = 50;
const DEFAULT_CONTEXT_EVIDENCE: u16 = 30;
const DEFAULT_CONTEXT_AFFECTED_TESTS: u16 = 20;
const DEFAULT_REVIEW_SYMBOLS_PER_FILE: u16 = 100;
const DEFAULT_REVIEW_ROOTS: u16 = 32;
const DEFAULT_REVIEW_GRAPH_NODES: u16 = 200;
const DEFAULT_REVIEW_EVIDENCE: u16 = 50;
const DEFAULT_REVIEW_AFFECTED_TESTS: u16 = 50;
const EVIDENCE_PRIORITY_EXACT_NAME: u8 = 0;
const EVIDENCE_PRIORITY_EXACT_PATH: u8 = 1;
const EVIDENCE_PRIORITY_EXACT_REFERENCE: u8 = 2;
const EVIDENCE_PRIORITY_COARSE_REFERENCE: u8 = 3;
const EVIDENCE_PRIORITY_BM25: u8 = 4;
const EVIDENCE_PRIORITY_SEMANTIC: u8 = 5;
const EVIDENCE_PRIORITY_GRAPH: u8 = 6;

const SYMBOL_LOOKUP_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    candidate_limit: 12,
    exact_limit: 50,
    traversal: TraversalBudget {
        max_depth: 1,
        max_nodes: 50,
    },
    evidence_limit: 20,
    affected_test_limit: 5,
};
const IMPLEMENTATION_TRACE_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    candidate_limit: 25,
    exact_limit: 75,
    traversal: TraversalBudget {
        max_depth: 3,
        max_nodes: 200,
    },
    evidence_limit: 40,
    affected_test_limit: 20,
};
const CHANGE_PLANNING_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    candidate_limit: 30,
    exact_limit: 100,
    traversal: TraversalBudget {
        max_depth: 4,
        max_nodes: 300,
    },
    evidence_limit: 50,
    affected_test_limit: 50,
};
const TEST_SELECTION_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    candidate_limit: 20,
    exact_limit: 75,
    traversal: TraversalBudget {
        max_depth: 4,
        max_nodes: 400,
    },
    evidence_limit: 30,
    affected_test_limit: 100,
};
const ERROR_DIAGNOSIS_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    candidate_limit: 30,
    exact_limit: 100,
    traversal: TraversalBudget {
        max_depth: 3,
        max_nodes: 250,
    },
    evidence_limit: 45,
    affected_test_limit: 40,
};
const ARCHITECTURE_SURVEY_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    candidate_limit: 40,
    exact_limit: 100,
    traversal: TraversalBudget {
        max_depth: 3,
        max_nodes: 300,
    },
    evidence_limit: 60,
    affected_test_limit: 20,
};
const DOCUMENTATION_LOOKUP_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    candidate_limit: 25,
    exact_limit: 50,
    traversal: TraversalBudget {
        max_depth: 1,
        max_nodes: 40,
    },
    evidence_limit: 30,
    affected_test_limit: 5,
};

/// Credential- and query-safe deterministic retrieval failure.
#[derive(Debug, Error)]
pub enum RetrievalError {
    /// A caller supplied an invalid bounded field. The field value is omitted.
    #[error("invalid {field} in deterministic retrieval request")]
    InvalidInput {
        /// Stable field name.
        field: &'static str,
    },
    /// PostgreSQL retrieval failed with Cartograph's redacted storage error.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Model-scoped pgvector retrieval failed without exposing source or credentials.
    #[error(transparent)]
    Semantic(#[from] SemanticStorageError),
}

/// Caller-owned assessment of whether the index matches the working tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexFreshness {
    /// The caller verified that the published generation matches current source.
    Current,
    /// The caller knows that source has changed since the generation was published.
    Stale,
    /// The caller could not establish the relationship to current source.
    Unknown,
}

/// Hard breadth/depth limits for one structural traversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TraversalBudget {
    max_depth: u8,
    max_nodes: u16,
}

impl TraversalBudget {
    /// Validate a traversal budget. Depth and node count must both be non-zero.
    /// # Errors
    ///
    /// Returns an error if `max_depth` or `max_nodes` is zero or exceeds the
    /// traversal hard limit.
    pub const fn new(max_depth: u8, max_nodes: u16) -> Result<Self, RetrievalError> {
        if max_depth == 0 || max_depth > MAX_DEPTH {
            return Err(invalid("max_depth"));
        }
        if max_nodes == 0 || max_nodes > MAX_GRAPH_NODES {
            return Err(invalid("max_nodes"));
        }
        Ok(Self {
            max_depth,
            max_nodes,
        })
    }

    /// Maximum number of graph hops.
    #[must_use]
    pub const fn max_depth(self) -> u8 {
        self.max_depth
    }

    /// Maximum number of non-root symbols returned.
    #[must_use]
    pub const fn max_nodes(self) -> u16 {
        self.max_nodes
    }
}

impl Default for TraversalBudget {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_nodes: DEFAULT_TRAVERSAL_NODES,
        }
    }
}

/// Named input for a context budget, avoiding positional-limit ambiguity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextBudgetInput {
    /// Maximum lexical candidates read before graph expansion.
    pub candidate_limit: u16,
    /// Maximum rows read by each exact anchor.
    pub exact_limit: u16,
    /// Structural traversal bounds.
    pub traversal: TraversalBudget,
    /// Maximum compact evidence items emitted.
    pub evidence_limit: u16,
    /// Maximum affected tests emitted.
    pub affected_test_limit: u16,
}

impl Default for ContextBudgetInput {
    fn default() -> Self {
        Self {
            candidate_limit: DEFAULT_CONTEXT_CANDIDATES,
            exact_limit: DEFAULT_CONTEXT_EXACT_RESULTS,
            traversal: TraversalBudget::default(),
            evidence_limit: DEFAULT_CONTEXT_EVIDENCE,
            affected_test_limit: DEFAULT_CONTEXT_AFFECTED_TESTS,
        }
    }
}

/// Bounded candidate and output limits for a compact evidence packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ContextBudget {
    candidate_limit: u16,
    exact_limit: u16,
    traversal: TraversalBudget,
    evidence_limit: u16,
    affected_test_limit: u16,
}

impl ContextBudget {
    /// Build a fully bounded packet budget.
    /// # Errors
    ///
    /// Returns an error if any candidate, exact, evidence, or affected-test
    /// limit is zero or exceeds its packet hard limit.
    pub fn new(input: ContextBudgetInput) -> Result<Self, RetrievalError> {
        validate_context_budget(&input)?;
        Ok(input.into())
    }

    /// Select a bounded default tuned for one deterministic coding-task intent.
    #[must_use]
    pub const fn for_intent(intent: TaskIntent) -> Self {
        match intent {
            TaskIntent::SymbolLookup => SYMBOL_LOOKUP_CONTEXT_BUDGET,
            TaskIntent::ImplementationTrace => IMPLEMENTATION_TRACE_CONTEXT_BUDGET,
            TaskIntent::ChangePlanning => CHANGE_PLANNING_CONTEXT_BUDGET,
            TaskIntent::TestSelection => TEST_SELECTION_CONTEXT_BUDGET,
            TaskIntent::ErrorDiagnosis => ERROR_DIAGNOSIS_CONTEXT_BUDGET,
            TaskIntent::ArchitectureSurvey => ARCHITECTURE_SURVEY_CONTEXT_BUDGET,
            TaskIntent::DocumentationLookup => DOCUMENTATION_LOOKUP_CONTEXT_BUDGET,
        }
    }

    /// Maximum lexical and semantic candidates each channel may prepare.
    #[must_use]
    pub const fn candidate_limit(self) -> u16 {
        self.candidate_limit
    }

    /// Maximum records read by each exact anchor.
    #[must_use]
    pub const fn exact_limit(self) -> u16 {
        self.exact_limit
    }

    /// Structural graph-expansion bounds.
    #[must_use]
    pub const fn traversal(self) -> TraversalBudget {
        self.traversal
    }

    /// Maximum compact evidence rows retained in a context packet.
    #[must_use]
    pub const fn evidence_limit(self) -> u16 {
        self.evidence_limit
    }

    /// Maximum affected-test candidates retained in a context packet.
    #[must_use]
    pub const fn affected_test_limit(self) -> u16 {
        self.affected_test_limit
    }
}

impl From<ContextBudgetInput> for ContextBudget {
    fn from(input: ContextBudgetInput) -> Self {
        Self {
            candidate_limit: input.candidate_limit,
            exact_limit: input.exact_limit,
            traversal: input.traversal,
            evidence_limit: input.evidence_limit,
            affected_test_limit: input.affected_test_limit,
        }
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        let input = ContextBudgetInput::default();
        Self {
            candidate_limit: input.candidate_limit,
            exact_limit: input.exact_limit,
            traversal: input.traversal,
            evidence_limit: input.evidence_limit,
            affected_test_limit: input.affected_test_limit,
        }
    }
}

/// Explicit exact evidence requested in addition to the task's BM25 query.
#[derive(Clone, PartialEq, Eq)]
pub enum ContextAnchor {
    /// Exact fully qualified declaration name.
    ExactName(String),
    /// Exact canonical project-relative path.
    ExactPath(NormalizedPath),
    /// Exact source reference text, including unresolved references.
    ExactReference(String),
}

impl fmt::Debug for ContextAnchor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::ExactName(_) => "ExactName",
            Self::ExactPath(_) => "ExactPath",
            Self::ExactReference(_) => "ExactReference",
        };
        write!(formatter, "{variant}(<redacted>)")
    }
}

/// One packet request. Its debug form intentionally redacts query and anchors.
pub struct ContextRequest {
    project_id: ProjectId,
    query: String,
    intent: TaskIntent,
    graph_direction: Option<ContextGraphDirection>,
    options: ContextRequestOptions,
    anchors: Vec<ContextAnchor>,
}

/// Non-sensitive execution options for one context request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextRequestOptions {
    freshness: IndexFreshness,
    budget: ContextBudget,
    search_mode: SearchMode,
    semantic_readiness: SemanticReadiness,
}

impl ContextRequestOptions {
    /// Pair caller-owned freshness with hard retrieval bounds.
    #[must_use]
    pub const fn new(freshness: IndexFreshness, budget: ContextBudget) -> Self {
        Self {
            freshness,
            budget,
            search_mode: SearchMode::Deterministic,
            semantic_readiness: SemanticReadiness::NotConfigured,
        }
    }

    /// Select deterministic, hybrid, or automatic channel policy.
    #[must_use]
    pub const fn with_search(
        mut self,
        mode: SearchMode,
        semantic_readiness: SemanticReadiness,
    ) -> Self {
        self.search_mode = mode;
        self.semantic_readiness = semantic_readiness;
        self
    }
}

impl ContextRequest {
    /// Create a bounded context request.
    /// # Errors
    ///
    /// Returns an error if `query` is empty, contains a NUL byte, or exceeds
    /// the maximum query byte length.
    pub fn new(
        project_id: ProjectId,
        query: impl Into<String>,
        options: ContextRequestOptions,
    ) -> Result<Self, RetrievalError> {
        let query = validate_query(query.into())?;
        let intent = TaskIntent::classify(&query);
        let graph_direction = ContextGraphDirection::classify(&query, intent);
        Ok(Self {
            project_id,
            query,
            intent,
            graph_direction,
            options,
            anchors: Vec::new(),
        })
    }

    /// Override automatic intent classification with a caller-owned typed intent.
    #[must_use]
    pub fn with_intent(mut self, intent: TaskIntent) -> Self {
        self.intent = intent;
        self.graph_direction = ContextGraphDirection::classify(&self.query, intent);
        self
    }

    /// Override automatic caller/callee selection for graph-bearing context intents.
    #[must_use]
    pub const fn with_graph_direction(mut self, direction: ContextGraphDirection) -> Self {
        self.graph_direction = Some(direction);
        self
    }

    /// Add one exact lookup anchor while preserving the request bounds.
    /// # Errors
    ///
    /// Returns an error if the request already has the maximum number of
    /// anchors or the anchor text violates its type-specific byte bound.
    pub fn with_anchor(mut self, anchor: ContextAnchor) -> Result<Self, RetrievalError> {
        if self.anchors.len() >= MAX_ANCHORS {
            return Err(invalid("anchors"));
        }
        validate_anchor(&anchor)?;
        self.anchors.push(anchor);
        Ok(self)
    }

    pub(crate) const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) const fn intent(&self) -> TaskIntent {
        self.intent
    }

    pub(crate) const fn graph_direction(&self) -> Option<ContextGraphDirection> {
        self.graph_direction
    }

    pub(crate) const fn freshness(&self) -> IndexFreshness {
        self.options.freshness
    }

    pub(crate) const fn budget(&self) -> ContextBudget {
        self.options.budget
    }

    pub(crate) const fn search_mode(&self) -> SearchMode {
        self.options.search_mode
    }

    pub(crate) const fn semantic_readiness(&self) -> SemanticReadiness {
        self.options.semantic_readiness
    }

    pub(crate) fn anchors(&self) -> &[ContextAnchor] {
        &self.anchors
    }
}

impl fmt::Debug for ContextRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextRequest")
            .field("project_id", &self.project_id)
            .field("query", &Redacted)
            .field("intent", &self.intent)
            .field("graph_direction", &self.graph_direction)
            .field("options", &self.options)
            .field("anchors", &RedactedCount(self.anchors.len()))
            .finish()
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

struct RedactedCount(usize);

impl fmt::Debug for RedactedCount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<redacted:{}>", self.0)
    }
}

/// Bounded borrowed text for an exact name or reference lookup.
#[derive(Clone, Copy)]
pub struct ExactTextQuery<'query> {
    value: &'query str,
    limit: u16,
}

impl<'query> ExactTextQuery<'query> {
    /// Validate one exact lookup without copying its text.
    /// # Errors
    ///
    /// Returns an error if `value` is empty, contains a NUL byte, exceeds the
    /// lookup-text bound, or `limit` is outside the exact-result bounds.
    pub fn new(value: &'query str, limit: u16) -> Result<Self, RetrievalError> {
        validate_lookup_text(value, limit)?;
        Ok(Self { value, limit })
    }

    pub(crate) const fn value(self) -> &'query str {
        self.value
    }

    pub(crate) const fn limit(self) -> u16 {
        self.limit
    }
}

impl fmt::Debug for ExactTextQuery<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactTextQuery")
            .field("value", &Redacted)
            .field("limit", &self.limit)
            .finish()
    }
}

/// Bounded exact-path lookup options.
#[derive(Clone, Copy)]
pub struct ExactPathQuery<'query> {
    path: &'query NormalizedPath,
    symbol_limit: u16,
}

impl fmt::Debug for ExactPathQuery<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactPathQuery")
            .field("path", &Redacted)
            .field("symbol_limit", &self.symbol_limit)
            .finish()
    }
}

impl<'query> ExactPathQuery<'query> {
    /// Validate the declaration bound for one canonical path.
    /// # Errors
    ///
    /// Returns an error if `symbol_limit` is zero or exceeds the maximum exact
    /// declarations returned for one path.
    pub fn new(path: &'query NormalizedPath, symbol_limit: u16) -> Result<Self, RetrievalError> {
        validate_limit(symbol_limit, MAX_EXACT_RESULTS, "symbol_limit")?;
        Ok(Self { path, symbol_limit })
    }

    pub(crate) const fn path(self) -> &'query NormalizedPath {
        self.path
    }

    pub(crate) const fn symbol_limit(self) -> u16 {
        self.symbol_limit
    }
}

/// Bounded, debug-redacted lexical query options.
pub struct LexicalQuery {
    query: String,
    limit: u16,
}

impl LexicalQuery {
    /// Validate a BM25 query before database work.
    /// # Errors
    ///
    /// Returns an error if the query is empty, contains a NUL byte, exceeds its
    /// byte bound, or `limit` is outside the BM25 candidate bounds.
    pub fn new(query: impl Into<String>, limit: u16) -> Result<Self, RetrievalError> {
        validate_limit(limit, MAX_CANDIDATES, "candidate_limit")?;
        Ok(Self {
            query: validate_query(query.into())?,
            limit,
        })
    }

    /// Expand a natural-language code search with bounded deterministic identifier aliases.
    /// # Errors
    ///
    /// Returns an error if the source query is empty, contains a NUL byte,
    /// exceeds its byte bound, or `limit` exceeds the candidate window.
    pub fn for_code_search(query: impl Into<String>, limit: u16) -> Result<Self, RetrievalError> {
        validate_limit(limit, MAX_CANDIDATES, "candidate_limit")?;
        let query = validate_query(query.into())?;
        Ok(Self {
            query: expand_code_query(&query),
            limit,
        })
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) const fn limit(&self) -> u16 {
        self.limit
    }
}

fn expand_code_query(query: &str) -> String {
    let normalized = query
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let mut seen = normalized
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut expanded = query.to_owned();
    for term in normalized.split_ascii_whitespace() {
        for alias in code_query_aliases(term) {
            if seen.contains(*alias)
                || expanded.len().saturating_add(alias.len()).saturating_add(1)
                    > CONTEXT_QUERY_MAXIMUM_BYTES
            {
                continue;
            }
            expanded.push(' ');
            expanded.push_str(alias);
            seen.insert((*alias).to_owned());
        }
    }
    expanded
}

fn code_query_aliases(term: &str) -> &'static [&'static str] {
    match term {
        "bm25" => &["generation_search_relation", "search_relation", "relation"],
        "table" | "tables" => &["relation", "catalog"],
        "check" | "checked" | "checking" | "checks" => &["require", "validate", "verify"],
        "run" | "running" | "runs" => &["execute", "query", "request"],
        "fresh" | "freshness" => &[
            "project_status",
            "source_revision",
            "digest_version",
            "scan_source",
        ],
        "checkout" | "checkouts" => &["source", "project_root"],
        "match" | "matched" | "matches" | "matching" => &["compare", "digest", "source_revision"],
        "immutable" => &["content_digest", "generation"],
        _ => &[],
    }
}

impl fmt::Debug for LexicalQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LexicalQuery")
            .field("query", &Redacted)
            .field("limit", &self.limit)
            .finish()
    }
}

/// Direction used by the public graph result and evidence provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraversalDirection {
    /// Source to target.
    Outgoing,
    /// Target back to source.
    Incoming,
}

/// Bounded graph request rooted at one or more symbols.
#[derive(Clone, Debug, PartialEq)]
pub struct TraversalRequest {
    project_id: ProjectId,
    roots: Vec<SymbolId>,
    budget: TraversalBudget,
    edge_kind: Option<EdgeKind>,
    minimum_confidence: f32,
    include_test_nodes: bool,
}

impl TraversalRequest {
    /// Validate roots and de-duplicate them without discarding caller relevance order.
    /// # Errors
    ///
    /// Returns an error if de-duplication leaves no roots or the distinct root
    /// count exceeds the traversal root bound.
    pub fn new(
        project_id: ProjectId,
        roots: impl IntoIterator<Item = SymbolId>,
        budget: TraversalBudget,
    ) -> Result<Self, RetrievalError> {
        let mut seen = BTreeSet::new();
        let roots = roots
            .into_iter()
            .filter(|root| seen.insert(root.clone()))
            .collect::<Vec<_>>();
        if roots.is_empty() || roots.len() > MAX_ROOTS {
            return Err(invalid("roots"));
        }
        Ok(Self {
            project_id,
            roots,
            budget,
            edge_kind: None,
            minimum_confidence: 0.0,
            include_test_nodes: true,
        })
    }

    /// Restrict traversal to one exact structural relation kind.
    #[must_use]
    pub const fn with_edge_kind(mut self, edge_kind: EdgeKind) -> Self {
        self.edge_kind = Some(edge_kind);
        self
    }

    /// Drop structural edges whose extractor confidence is below this exact threshold.
    /// # Errors
    ///
    /// Returns an error if `minimum` is not finite or lies outside the
    /// inclusive extractor-confidence range from zero to one.
    pub fn with_minimum_confidence(mut self, minimum: f32) -> Result<Self, RetrievalError> {
        validate_minimum_confidence(minimum)?;
        self.minimum_confidence = minimum;
        Ok(self)
    }

    /// Decide whether conventional test-path nodes may enter graph expansion.
    #[must_use]
    pub const fn with_test_nodes(mut self, include: bool) -> Self {
        self.include_test_nodes = include;
        self
    }

    pub(crate) const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// De-duplicated roots in deterministic caller-provided relevance order.
    #[must_use]
    pub fn roots(&self) -> &[SymbolId] {
        &self.roots
    }

    pub(crate) const fn budget(&self) -> TraversalBudget {
        self.budget
    }

    pub(crate) const fn edge_kind(&self) -> Option<EdgeKind> {
        self.edge_kind
    }

    pub(crate) const fn minimum_confidence(&self) -> f32 {
        self.minimum_confidence
    }

    pub(crate) const fn include_test_nodes(&self) -> bool {
        self.include_test_nodes
    }
}

/// Bounded shortest-path request over outgoing structural graph edges.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphPathRequest {
    project_id: ProjectId,
    start: SymbolId,
    target: SymbolId,
    budget: TraversalBudget,
    edge_kind: Option<EdgeKind>,
    minimum_confidence: f32,
}

/// Exact endpoints, project, and work budget for one shortest-path request.
pub struct GraphPathRequestInput {
    /// Stable project ID for this record.
    pub project_id: ProjectId,
    /// Start for this record.
    pub start: SymbolId,
    /// Target for this record.
    pub target: SymbolId,
    /// Budget for this record.
    pub budget: TraversalBudget,
}

/// Model-scoped stored-vector neighbor request for one exact source symbol.
#[derive(Clone, Debug, PartialEq)]
pub struct SimilarRequest {
    project_id: ProjectId,
    source_symbol_id: SymbolId,
    model_id: Option<ModelId>,
    limit: u16,
    minimum_score: f64,
    same_language: bool,
}

impl SimilarRequest {
    /// Build a bounded semantic-neighbor request with a permissive 0.3 score floor.
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or greater than the semantic
    /// neighbor result maximum.
    pub fn new(
        project_id: ProjectId,
        source_symbol_id: SymbolId,
        limit: u16,
    ) -> Result<Self, RetrievalError> {
        if limit == 0 || limit > 50 {
            return Err(invalid("similar_limit"));
        }
        Ok(Self {
            project_id,
            source_symbol_id,
            model_id: None,
            limit,
            minimum_score: 0.3,
            same_language: false,
        })
    }

    /// Select one exact active model when more than one source embedding exists.
    #[must_use]
    pub fn with_model_id(mut self, model_id: ModelId) -> Self {
        self.model_id = Some(model_id);
        self
    }

    /// Set an inclusive cosine-similarity floor in the 0..1 range.
    /// # Errors
    ///
    /// Returns an error if `score` is not finite or lies outside the inclusive
    /// cosine-similarity range from zero to one.
    pub fn with_minimum_score(mut self, score: f64) -> Result<Self, RetrievalError> {
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(invalid("minimum_score"));
        }
        self.minimum_score = score;
        Ok(self)
    }

    /// Restrict neighbors to the source symbol's indexed language.
    #[must_use]
    pub const fn with_same_language(mut self, same_language: bool) -> Self {
        self.same_language = same_language;
        self
    }

    pub(crate) const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub(crate) const fn source_symbol_id(&self) -> &SymbolId {
        &self.source_symbol_id
    }

    pub(crate) const fn model_id(&self) -> Option<&ModelId> {
        self.model_id.as_ref()
    }

    pub(crate) const fn limit(&self) -> u16 {
        self.limit
    }

    pub(crate) const fn minimum_score(&self) -> f64 {
        self.minimum_score
    }

    pub(crate) const fn same_language(&self) -> bool {
        self.same_language
    }
}

impl GraphPathRequest {
    /// Bind exact endpoints to one bounded, generation-fenced search.
    #[must_use]
    pub fn new(input: GraphPathRequestInput) -> Self {
        let GraphPathRequestInput {
            project_id,
            start,
            target,
            budget,
        } = input;
        Self {
            project_id,
            start,
            target,
            budget,
            edge_kind: None,
            minimum_confidence: 0.0,
        }
    }

    /// Restrict the path to one exact structural relation kind.
    #[must_use]
    pub const fn with_edge_kind(mut self, edge_kind: EdgeKind) -> Self {
        self.edge_kind = Some(edge_kind);
        self
    }

    /// Drop path candidates below an exact extractor-confidence threshold.
    /// # Errors
    ///
    /// Returns an error if `minimum` is not finite or lies outside the
    /// inclusive extractor-confidence range from zero to one.
    pub fn with_minimum_confidence(mut self, minimum: f32) -> Result<Self, RetrievalError> {
        validate_minimum_confidence(minimum)?;
        self.minimum_confidence = minimum;
        Ok(self)
    }

    pub(crate) const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Exact source endpoint.
    #[must_use]
    pub const fn start(&self) -> &SymbolId {
        &self.start
    }

    /// Exact destination endpoint.
    #[must_use]
    pub const fn target(&self) -> &SymbolId {
        &self.target
    }

    pub(crate) const fn budget(&self) -> TraversalBudget {
        self.budget
    }

    pub(crate) const fn edge_kind(&self) -> Option<EdgeKind> {
        self.edge_kind
    }

    pub(crate) const fn minimum_confidence(&self) -> f32 {
        self.minimum_confidence
    }
}

/// Exact path result, including declarations when the file has any.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExactPathResult {
    file: CurrentFileRecord,
    symbols: Vec<CurrentSymbolRecord>,
}

/// Bounded path-ordered current-generation file inventory request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInventoryQuery {
    directory: Option<NormalizedPath>,
    language: Option<SourceLanguage>,
    limit: u16,
}

impl FileInventoryQuery {
    /// Build a bounded inventory request.
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or exceeds the bounded file
    /// inventory result maximum.
    pub const fn new(limit: u16) -> Result<Self, RetrievalError> {
        if limit == 0 || limit > MAX_FILE_INVENTORY_RESULTS {
            return Err(invalid("file_limit"));
        }
        Ok(Self {
            directory: None,
            language: None,
            limit,
        })
    }

    /// Restrict the inventory to one exact directory subtree.
    #[must_use]
    pub fn within_directory(mut self, directory: NormalizedPath) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Restrict the inventory to one registered language.
    #[must_use]
    pub const fn with_language(mut self, language: SourceLanguage) -> Self {
        self.language = Some(language);
        self
    }

    pub(crate) const fn directory(&self) -> Option<&NormalizedPath> {
        self.directory.as_ref()
    }

    pub(crate) const fn language(&self) -> Option<SourceLanguage> {
        self.language
    }

    pub(crate) const fn limit(&self) -> u16 {
        self.limit
    }
}

/// Bounded file inventory with explicit output truncation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileInventoryResult {
    files: Vec<CurrentFileRecord>,
    truncated: bool,
}

/// Bounded typed entry-point discovery request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryPointsQuery {
    bucket: Option<EntryPointBucket>,
    limit: u16,
}

impl EntryPointsQuery {
    /// Build an all-bucket request with a per-bucket limit.
    /// # Errors
    ///
    /// Returns an error if the per-bucket `limit` is zero or exceeds the
    /// bounded entry-point result maximum.
    pub const fn new(limit: u16) -> Result<Self, RetrievalError> {
        if limit == 0 || limit > MAX_ENTRY_POINT_RESULTS {
            return Err(invalid("entry_point_limit"));
        }
        Ok(Self {
            bucket: None,
            limit,
        })
    }

    /// Restrict discovery to one exact category.
    #[must_use]
    pub const fn with_bucket(mut self, bucket: EntryPointBucket) -> Self {
        self.bucket = Some(bucket);
        self
    }

    pub(crate) const fn bucket(self) -> Option<EntryPointBucket> {
        self.bucket
    }

    pub(crate) const fn limit(self) -> u16 {
        self.limit
    }
}

/// Generation-fenced entry-point pages with exact totals and truncation per category.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EntryPointsResult {
    generation_id: Option<GenerationId>,
    buckets: Vec<CurrentEntryPointPage>,
}

impl EntryPointsResult {
    pub(crate) const fn new(
        generation_id: Option<GenerationId>,
        buckets: Vec<CurrentEntryPointPage>,
    ) -> Self {
        Self {
            generation_id,
            buckets,
        }
    }

    /// Immutable generation represented by every page, if one is published.
    #[must_use]
    pub const fn generation_id(&self) -> Option<&GenerationId> {
        self.generation_id.as_ref()
    }

    /// Stable bucket-ordered pages. Empty categories remain present for an all-bucket request.
    #[must_use]
    pub fn buckets(&self) -> &[CurrentEntryPointPage] {
        &self.buckets
    }

    /// Whether any category was truncated by the per-bucket limit.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.buckets.iter().any(CurrentEntryPointPage::truncated)
    }
}

impl FileInventoryResult {
    pub(crate) const fn new(files: Vec<CurrentFileRecord>, truncated: bool) -> Self {
        Self { files, truncated }
    }

    /// Canonically path-ordered current-generation files.
    #[must_use]
    pub fn files(&self) -> &[CurrentFileRecord] {
        &self.files
    }

    /// True when the result limit omitted additional files.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Exact file and inclusive one-based source range request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRangeQuery {
    path: NormalizedPath,
    start_line: u32,
    end_line: u32,
    limit: u16,
}

/// Exact path, inclusive line range, and result bound for one source lookup.
pub struct SourceRangeQueryInput {
    /// Project-relative path for this record.
    pub path: NormalizedPath,
    /// One-based starting source line.
    pub start_line: u32,
    /// One-based ending source line.
    pub end_line: u32,
    /// Maximum number of results returned by this bounded request.
    pub limit: u16,
}

impl SourceRangeQuery {
    /// Validate an inclusive range and bounded result count before database work.
    /// # Errors
    ///
    /// Returns an error if the range is not one-based and ordered, spans too
    /// many lines, or `limit` is outside the source-range result bounds.
    pub fn new(input: SourceRangeQueryInput) -> Result<Self, RetrievalError> {
        let SourceRangeQueryInput {
            path,
            start_line,
            end_line,
            limit,
        } = input;
        let line_count = end_line.saturating_sub(start_line).saturating_add(1);
        if start_line == 0 || end_line < start_line || line_count > MAX_SOURCE_RANGE_LINES {
            return Err(invalid("source_range"));
        }
        if limit == 0 || limit > MAX_SOURCE_RANGE_RESULTS {
            return Err(invalid("source_range_limit"));
        }
        Ok(Self {
            path,
            start_line,
            end_line,
            limit,
        })
    }

    pub(crate) const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    pub(crate) const fn start_line(&self) -> u32 {
        self.start_line
    }

    pub(crate) const fn end_line(&self) -> u32 {
        self.end_line
    }

    pub(crate) const fn limit(&self) -> u16 {
        self.limit
    }
}

/// Exact current file plus smallest-first symbols overlapping one source range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SourceRangeResult {
    file: CurrentFileRecord,
    symbols: Vec<CurrentSymbolRecord>,
    truncated: bool,
}

impl SourceRangeResult {
    pub(crate) const fn new(
        file: CurrentFileRecord,
        symbols: Vec<CurrentSymbolRecord>,
        truncated: bool,
    ) -> Self {
        Self {
            file,
            symbols,
            truncated,
        }
    }

    /// Exact file containing the requested range.
    #[must_use]
    pub const fn file(&self) -> &CurrentFileRecord {
        &self.file
    }

    /// Smallest enclosing or overlapping declarations first.
    #[must_use]
    pub fn symbols(&self) -> &[CurrentSymbolRecord] {
        &self.symbols
    }

    /// True when the result limit omitted additional overlapping symbols.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl ExactPathResult {
    pub(crate) const fn new(file: CurrentFileRecord, symbols: Vec<CurrentSymbolRecord>) -> Self {
        Self { file, symbols }
    }

    /// Exact current-generation file.
    #[must_use]
    pub const fn file(&self) -> &CurrentFileRecord {
        &self.file
    }

    /// Declarations ordered by source position.
    #[must_use]
    pub fn symbols(&self) -> &[CurrentSymbolRecord] {
        &self.symbols
    }
}

/// One graph hop that justified including a symbol.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraversalHop {
    pub(crate) from_symbol_id: SymbolId,
    pub(crate) to_symbol_id: SymbolId,
    pub(crate) edge_source_symbol_id: SymbolId,
    pub(crate) edge_target_symbol_id: SymbolId,
    pub(crate) edge_kind: String,
    pub(crate) confidence: f32,
    pub(crate) provenance: String,
    pub(crate) site_count: u32,
}

impl TraversalHop {
    /// Stored edge source, independent of traversal direction.
    #[must_use]
    pub const fn edge_source_symbol_id(&self) -> &SymbolId {
        &self.edge_source_symbol_id
    }

    /// Stored edge target, independent of traversal direction.
    #[must_use]
    pub const fn edge_target_symbol_id(&self) -> &SymbolId {
        &self.edge_target_symbol_id
    }

    /// Symbol at the preceding depth.
    #[must_use]
    pub const fn from_symbol_id(&self) -> &SymbolId {
        &self.from_symbol_id
    }

    /// Newly discovered symbol.
    #[must_use]
    pub const fn to_symbol_id(&self) -> &SymbolId {
        &self.to_symbol_id
    }

    /// Stable relation name.
    #[must_use]
    pub fn edge_kind(&self) -> &str {
        &self.edge_kind
    }

    /// Extractor confidence for this relation.
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

/// Hydrated symbol reached by a structural traversal.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraversalNode {
    symbol: CurrentSymbolRecord,
    depth: u8,
    via: TraversalHop,
}

impl TraversalNode {
    pub(crate) const fn new(symbol: CurrentSymbolRecord, depth: u8, via: TraversalHop) -> Self {
        Self { symbol, depth, via }
    }

    /// Current-generation symbol.
    #[must_use]
    pub const fn symbol(&self) -> &CurrentSymbolRecord {
        &self.symbol
    }

    /// One-based graph distance from the nearest root.
    #[must_use]
    pub const fn depth(&self) -> u8 {
        self.depth
    }

    /// First deterministic hop that discovered this symbol.
    #[must_use]
    pub const fn via(&self) -> &TraversalHop {
        &self.via
    }
}

/// Deterministic bounded callers, callees, or impact result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraversalResult {
    pub(crate) direction: TraversalDirection,
    pub(crate) roots: Vec<SymbolId>,
    pub(crate) nodes: Vec<TraversalNode>,
    pub(crate) truncated: bool,
}

/// Incoming and outgoing dependency traversals fenced to one immutable generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BidirectionalTraversalResult {
    incoming: TraversalResult,
    outgoing: TraversalResult,
    truncated: bool,
}

impl BidirectionalTraversalResult {
    pub(crate) const fn new(incoming: TraversalResult, outgoing: TraversalResult) -> Self {
        let truncated = incoming.truncated || outgoing.truncated;
        Self {
            incoming,
            outgoing,
            truncated,
        }
    }

    /// Incoming dependency traversal.
    #[must_use]
    pub const fn incoming(&self) -> &TraversalResult {
        &self.incoming
    }

    /// Outgoing dependency traversal.
    #[must_use]
    pub const fn outgoing(&self) -> &TraversalResult {
        &self.outgoing
    }

    /// Whether either bounded direction omitted candidates.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// One ordered symbol in a shortest dependency path.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphPathStep {
    symbol: CurrentSymbolRecord,
    via: Option<TraversalHop>,
}

impl GraphPathStep {
    pub(crate) const fn new(symbol: CurrentSymbolRecord, via: Option<TraversalHop>) -> Self {
        Self { symbol, via }
    }

    /// Current-generation symbol at this path position.
    #[must_use]
    pub const fn symbol(&self) -> &CurrentSymbolRecord {
        &self.symbol
    }

    /// Edge from the previous path symbol, absent only for the start endpoint.
    #[must_use]
    pub const fn via(&self) -> Option<&TraversalHop> {
        self.via.as_ref()
    }
}

/// Generation-fenced shortest-path result with explicit budget truncation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphPathResult {
    start: SymbolId,
    target: SymbolId,
    path: Option<Vec<GraphPathStep>>,
    truncated: bool,
}

pub(crate) struct GraphPathResultInput {
    pub(crate) start: SymbolId,
    pub(crate) target: SymbolId,
    pub(crate) path: Option<Vec<GraphPathStep>>,
    pub(crate) truncated: bool,
}

impl GraphPathResult {
    pub(crate) fn new(input: GraphPathResultInput) -> Self {
        let GraphPathResultInput {
            start,
            target,
            path,
            truncated,
        } = input;
        Self {
            start,
            target,
            path,
            truncated,
        }
    }

    /// Exact source endpoint requested by the caller.
    #[must_use]
    pub const fn start(&self) -> &SymbolId {
        &self.start
    }

    /// Exact destination endpoint requested by the caller.
    #[must_use]
    pub const fn target(&self) -> &SymbolId {
        &self.target
    }

    /// Ordered shortest path, including both endpoints, when one was found.
    #[must_use]
    pub fn path(&self) -> Option<&[GraphPathStep]> {
        self.path.as_deref()
    }

    /// Whether an edge, node, or depth bound may have hidden a path.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl TraversalResult {
    /// Traversal direction.
    #[must_use]
    pub const fn direction(&self) -> TraversalDirection {
        self.direction
    }

    /// Sorted root identities.
    #[must_use]
    pub fn roots(&self) -> &[SymbolId] {
        &self.roots
    }

    /// Nodes ordered by distance, path, position, and identity.
    #[must_use]
    pub fn nodes(&self) -> &[TraversalNode] {
        &self.nodes
    }

    /// Whether an edge or node bound cut off candidates.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// One test declaration affected through reverse structural traversal.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AffectedTest {
    symbol: CurrentSymbolRecord,
    distance: u8,
    reason: String,
}

/// Bounded affected-test discovery result with explicit truncation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AffectedTestsResult {
    tests: Vec<AffectedTest>,
    truncated: bool,
}

impl AffectedTestsResult {
    pub(crate) const fn new(tests: Vec<AffectedTest>, truncated: bool) -> Self {
        Self { tests, truncated }
    }

    /// Deterministically ordered test candidates.
    #[must_use]
    pub fn tests(&self) -> &[AffectedTest] {
        &self.tests
    }

    /// Whether the traversal or output limit omitted candidates.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl AffectedTest {
    pub(crate) fn new(symbol: CurrentSymbolRecord, distance: u8, reason: String) -> Self {
        Self {
            symbol,
            distance,
            reason,
        }
    }

    /// Test symbol/file evidence.
    #[must_use]
    pub const fn symbol(&self) -> &CurrentSymbolRecord {
        &self.symbol
    }

    /// Graph distance from the changed root.
    #[must_use]
    pub const fn distance(&self) -> u8 {
        self.distance
    }

    /// Stable relation or path-classification reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Why an evidence item was admitted to a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceReason {
    /// Exact symbol-name anchor.
    ExactName,
    /// Exact canonical-path anchor.
    ExactPath,
    /// Exact source-reference anchor.
    ExactReference,
    /// Validated legacy reference anchor whose byte span is intentionally coarse.
    CoarseReference,
    /// Current-generation `ParadeDB` BM25 candidate.
    Bm25,
    /// Current-generation semantic candidate.
    Semantic,
    /// Bounded structural expansion from an anchored candidate.
    Graph,
}

/// Machine-readable precision of source coordinates retained for reference evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceSpanPrecision {
    /// Start/end identify the exact source token.
    Exact,
    /// Start/end identify a validated point or line anchor.
    CoarsePoint,
    /// Start/end conservatively reuse the lexical owner span.
    CoarseOwner,
}

/// Exact persisted provenance for one retained source-reference anchor.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReferenceEvidence {
    #[serde(flatten)]
    details: ReferenceEvidenceDetails,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct ReferenceEvidenceDetails {
    reference_id: u64,
    owner_symbol_id: Option<SymbolId>,
    target_symbol_id: Option<SymbolId>,
    start_byte: u64,
    end_byte: u64,
    span_precision: ReferenceSpanPrecision,
    confidence: f32,
    provenance: String,
    represented_site_count: u64,
}

impl ReferenceEvidence {
    /// Stable current-generation reference-row identity.
    #[must_use]
    pub const fn reference_id(&self) -> u64 {
        self.details.reference_id
    }

    /// Lexical owner of the reference, when known.
    #[must_use]
    pub const fn owner_symbol_id(&self) -> Option<&SymbolId> {
        self.details.owner_symbol_id.as_ref()
    }

    /// Resolved target of the reference, when known.
    #[must_use]
    pub const fn target_symbol_id(&self) -> Option<&SymbolId> {
        self.details.target_symbol_id.as_ref()
    }

    /// Inclusive source byte offset.
    #[must_use]
    pub const fn start_byte(&self) -> u64 {
        self.details.start_byte
    }

    /// Exclusive source byte offset.
    #[must_use]
    pub const fn end_byte(&self) -> u64 {
        self.details.end_byte
    }

    /// Whether the byte span is exact or a bounded legacy anchor.
    #[must_use]
    pub const fn span_precision(&self) -> ReferenceSpanPrecision {
        self.details.span_precision
    }

    /// Extractor confidence in the reference resolution.
    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.details.confidence
    }

    /// Stable extractor or migration provenance label.
    #[must_use]
    pub fn provenance(&self) -> &str {
        &self.details.provenance
    }

    /// Exact number of source sites represented by this retained anchor.
    #[must_use]
    pub const fn represented_site_count(&self) -> u64 {
        self.details.represented_site_count
    }
}

/// Typed structural-edge proof retained for one graph-expanded symbol.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphEvidence {
    from_symbol_id: SymbolId,
    to_symbol_id: SymbolId,
    depth: u8,
    edge_kind: String,
    confidence: f32,
    provenance: String,
    site_count: u64,
}

impl GraphEvidence {
    /// Symbol at the preceding traversal depth.
    #[must_use]
    pub const fn from_symbol_id(&self) -> &SymbolId {
        &self.from_symbol_id
    }

    /// Symbol admitted by this edge.
    #[must_use]
    pub const fn to_symbol_id(&self) -> &SymbolId {
        &self.to_symbol_id
    }

    /// Traversal depth at which the symbol was first admitted.
    #[must_use]
    pub const fn depth(&self) -> u8 {
        self.depth
    }

    /// Stable structural relation name.
    #[must_use]
    pub fn edge_kind(&self) -> &str {
        &self.edge_kind
    }

    /// Extractor confidence for the retained relation.
    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    /// Stable extractor or migration provenance label.
    #[must_use]
    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    /// Exact number of source relation sites represented by the edge.
    #[must_use]
    pub const fn site_count(&self) -> u64 {
        self.site_count
    }
}

/// Compact serializable evidence with explicit component provenance.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvidenceItem {
    #[serde(flatten)]
    details: EvidenceDetails,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct EvidenceDetails {
    generation_id: GenerationId,
    file_id: Option<FileId>,
    symbol_id: Option<SymbolId>,
    document_id: Option<DocumentId>,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<SourceLanguage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    document_kind: Option<DocumentKind>,
    qualified_name: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
    reasons: Vec<EvidenceReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fused_rank: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reciprocal_rank_score: Option<f64>,
    bm25_rank: Option<u16>,
    bm25_score: Option<f64>,
    bm25_components: Vec<SearchComponent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<ReferenceEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph: Option<GraphEvidence>,
    /// Advisory decision-provider probability that reading this candidate
    /// helps the task; present only on ranked retrieval evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    decision_relevance: Option<f64>,
}

impl EvidenceItem {
    /// Advisory decision-provider relevance, when the packet was ranked.
    #[must_use]
    pub const fn decision_relevance(&self) -> Option<f64> {
        self.details.decision_relevance
    }

    /// Whether this item is only a BM25 or semantic retrieval candidate, not an
    /// exact caller anchor or structural expansion.
    #[must_use]
    pub fn is_retrieval_candidate(&self) -> bool {
        !self.details.reasons.is_empty()
            && self
                .details
                .reasons
                .iter()
                .all(|reason| matches!(reason, EvidenceReason::Bm25 | EvidenceReason::Semantic))
    }

    pub(crate) fn set_decision_relevance(&mut self, relevance: f64) {
        self.details.decision_relevance = Some(relevance);
    }

    /// Published generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.details.generation_id
    }

    /// Canonical project-relative path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.details.path
    }

    /// Typed source language retained from exact or search-document evidence.
    #[must_use]
    pub const fn language(&self) -> Option<SourceLanguage> {
        self.details.language
    }

    /// Typed search-document role when this item came from a document channel.
    #[must_use]
    pub const fn document_kind(&self) -> Option<DocumentKind> {
        self.details.document_kind
    }

    /// Declaration/reference name when available.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.details.qualified_name
    }

    /// One-based declaration start line when the evidence has an exact symbol span.
    #[must_use]
    pub const fn start_line(&self) -> Option<u32> {
        self.details.start_line
    }

    /// One-based declaration end line when the evidence has an exact symbol span.
    #[must_use]
    pub const fn end_line(&self) -> Option<u32> {
        self.details.end_line
    }

    /// Ordered admission reasons.
    #[must_use]
    pub fn reasons(&self) -> &[EvidenceReason] {
        &self.details.reasons
    }

    /// One-based deterministic rank after lexical/semantic fusion.
    #[must_use]
    pub const fn fused_rank(&self) -> Option<u16> {
        self.details.fused_rank
    }

    /// Additive reciprocal-rank score retained for relevance explanation.
    #[must_use]
    pub const fn reciprocal_rank_score(&self) -> Option<f64> {
        self.details.reciprocal_rank_score
    }

    /// One-based BM25 rank within this packet's candidate channel.
    #[must_use]
    pub const fn bm25_rank(&self) -> Option<u16> {
        self.details.bm25_rank
    }

    /// Native BM25 score retained for same-channel explanation.
    #[must_use]
    pub const fn bm25_score(&self) -> Option<f64> {
        self.details.bm25_score
    }

    /// Qualified-name/code/natural-text fields contributing to BM25 matching.
    #[must_use]
    pub fn bm25_components(&self) -> &[SearchComponent] {
        &self.details.bm25_components
    }

    /// Symbol identity when this is symbol-backed evidence.
    #[must_use]
    pub const fn symbol_id(&self) -> Option<&SymbolId> {
        self.details.symbol_id.as_ref()
    }

    /// Search-document identity when this came from BM25.
    #[must_use]
    pub const fn document_id(&self) -> Option<&DocumentId> {
        self.details.document_id.as_ref()
    }

    /// File identity when this is file-backed evidence.
    #[must_use]
    pub const fn file_id(&self) -> Option<&FileId> {
        self.details.file_id.as_ref()
    }

    /// Exact source-reference provenance when this item is a retained reference anchor.
    #[must_use]
    pub const fn reference(&self) -> Option<&ReferenceEvidence> {
        self.details.reference.as_ref()
    }

    /// Typed edge proof when this item was admitted by graph traversal.
    #[must_use]
    pub const fn graph(&self) -> Option<&GraphEvidence> {
        self.details.graph.as_ref()
    }
}

pub(crate) fn evidence_from_symbol(
    symbol: &CurrentSymbolRecord,
    reason: EvidenceReason,
) -> EvidenceItem {
    let mut item = evidence_base(
        symbol.generation_id().clone(),
        symbol.path().as_str(),
        symbol.qualified_name(),
    );
    item.details.file_id = Some(symbol.file_id().clone());
    item.details.symbol_id = Some(symbol.symbol_id().clone());
    item.details.language = SourceLanguage::from_stable_str(symbol.language());
    item.details.document_kind = Some(if is_test_path(symbol.path().as_str()) {
        DocumentKind::Test
    } else {
        DocumentKind::Symbol
    });
    item.details.start_line = Some(symbol.start_line());
    item.details.end_line = Some(symbol.end_line());
    item.details.reasons.push(reason);
    item
}

pub(crate) fn evidence_from_file(file: &CurrentFileRecord) -> EvidenceItem {
    let mut item = evidence_base(
        file.generation_id().clone(),
        file.path().as_str(),
        String::new(),
    );
    item.details.file_id = Some(file.file_id().clone());
    item.details.language = SourceLanguage::from_stable_str(file.language());
    item.details.document_kind = Some(if is_test_path(file.path().as_str()) {
        DocumentKind::Test
    } else {
        DocumentKind::File
    });
    item.details.reasons.push(EvidenceReason::ExactPath);
    item
}

pub(crate) fn evidence_from_reference(reference: &CurrentReferenceRecord) -> EvidenceItem {
    let mut item = evidence_base(
        reference.generation_id().clone(),
        reference.path().as_str(),
        reference.reference_name(),
    );
    item.details.file_id = Some(reference.file_id().clone());
    item.details.symbol_id = reference
        .target_symbol_id()
        .or(reference.owner_symbol_id())
        .cloned();
    let span_precision = reference_span_precision(reference.span_precision());
    item.details.reasons.push(reference_reason(span_precision));
    item.details.reference = Some(ReferenceEvidence {
        details: ReferenceEvidenceDetails {
            reference_id: reference.reference_id(),
            owner_symbol_id: reference.owner_symbol_id().cloned(),
            target_symbol_id: reference.target_symbol_id().cloned(),
            start_byte: reference.start_byte(),
            end_byte: reference.end_byte(),
            span_precision,
            confidence: reference.confidence(),
            provenance: reference.provenance().to_owned(),
            represented_site_count: u64::from(reference.site_count()),
        },
    });
    item
}

pub(crate) fn evidence_from_fused_item(fused: &FusedSearchItem) -> EvidenceItem {
    let document = fused.document();
    let mut item = evidence_base(
        document.generation_id().clone(),
        document.path().as_str(),
        document.qualified_name(),
    );
    item.details.file_id = document.file_id().cloned();
    item.details.symbol_id = document.symbol_id().cloned();
    item.details.document_id = Some(document.document_id().clone());
    item.details.language = Some(document.language());
    item.details.document_kind = Some(document.document_kind());
    item.details.fused_rank = Some(fused.rank());
    item.details.reciprocal_rank_score = Some(fused.reciprocal_rank_score());
    for contribution in fused.contributions() {
        match contribution.channel() {
            RetrievalChannel::Lexical => {
                item.details.reasons.push(EvidenceReason::Bm25);
                item.details.bm25_rank =
                    minimum_option(item.details.bm25_rank, Some(contribution.rank()));
                item.details.bm25_score =
                    maximum_score(item.details.bm25_score, Some(contribution.raw_score()));
                item.details.bm25_components.extend(
                    contribution
                        .lexical_components()
                        .iter()
                        .copied()
                        .map(search_component),
                );
            }
            RetrievalChannel::Semantic => item.details.reasons.push(EvidenceReason::Semantic),
        }
    }
    item.details.reasons.sort_unstable();
    item.details.reasons.dedup();
    item.details.bm25_components.sort_unstable();
    item.details.bm25_components.dedup();
    item
}

pub(crate) fn evidence_from_traversal_node(node: &TraversalNode) -> EvidenceItem {
    let mut item = evidence_from_symbol(node.symbol(), EvidenceReason::Graph);
    item.details.graph = Some(GraphEvidence {
        from_symbol_id: node.via().from_symbol_id().clone(),
        to_symbol_id: node.via().to_symbol_id().clone(),
        depth: node.depth(),
        edge_kind: node.via().edge_kind().to_owned(),
        confidence: node.via().confidence(),
        provenance: node.via().provenance().to_owned(),
        site_count: u64::from(node.via().site_count()),
    });
    item
}

pub(crate) const fn evidence_start_line(item: &EvidenceItem) -> Option<u32> {
    item.details.start_line
}

pub(crate) fn evidence_priority(item: &EvidenceItem) -> u8 {
    let anchor_priority = item
        .details
        .reasons
        .iter()
        .filter_map(|reason| match reason {
            EvidenceReason::ExactName => Some(EVIDENCE_PRIORITY_EXACT_NAME),
            EvidenceReason::ExactPath => Some(EVIDENCE_PRIORITY_EXACT_PATH),
            EvidenceReason::ExactReference => Some(EVIDENCE_PRIORITY_EXACT_REFERENCE),
            EvidenceReason::CoarseReference => Some(EVIDENCE_PRIORITY_COARSE_REFERENCE),
            EvidenceReason::Bm25 | EvidenceReason::Semantic | EvidenceReason::Graph => None,
        })
        .min();
    if let Some(priority) = anchor_priority {
        return priority;
    }
    if item.details.fused_rank.is_some() {
        return EVIDENCE_PRIORITY_BM25;
    }
    if item.details.reasons.contains(&EvidenceReason::Semantic) {
        return EVIDENCE_PRIORITY_SEMANTIC;
    }
    if item.details.reasons.contains(&EvidenceReason::Graph) {
        return EVIDENCE_PRIORITY_GRAPH;
    }
    u8::MAX
}

pub(crate) fn merge_evidence(item: &mut EvidenceItem, mut other: EvidenceItem) {
    let incoming_fused_metadata_is_stronger =
        match (item.details.fused_rank, other.details.fused_rank) {
            (None, Some(_)) => true,
            (Some(retained), Some(incoming)) => incoming < retained,
            _ => false,
        };
    item.details.reasons.append(&mut other.details.reasons);
    item.details.reasons.sort_unstable();
    item.details.reasons.dedup();
    item.details
        .bm25_components
        .append(&mut other.details.bm25_components);
    item.details.bm25_components.sort_unstable();
    item.details.bm25_components.dedup();
    item.details.bm25_rank = minimum_option(item.details.bm25_rank, other.details.bm25_rank);
    item.details.bm25_score = maximum_score(item.details.bm25_score, other.details.bm25_score);
    item.details.fused_rank = minimum_option(item.details.fused_rank, other.details.fused_rank);
    item.details.reciprocal_rank_score = maximum_score(
        item.details.reciprocal_rank_score,
        other.details.reciprocal_rank_score,
    );
    if incoming_fused_metadata_is_stronger {
        item.details.language = other.details.language;
        item.details.document_kind = other.details.document_kind;
        if other.details.document_id.is_some() {
            item.details
                .document_id
                .clone_from(&other.details.document_id);
        }
    } else {
        if item.details.language.is_none() {
            item.details.language = other.details.language;
        }
        if item.details.document_kind.is_none() {
            item.details.document_kind = other.details.document_kind;
        }
    }
    if item.details.document_id.is_none() {
        item.details.document_id = other.details.document_id;
    }
    merge_reference_evidence(&mut item.details.reference, other.details.reference);
    merge_graph_evidence(&mut item.details.graph, other.details.graph);
}

pub(crate) fn evidence_key(item: &EvidenceItem) -> String {
    if let Some(reference) = &item.details.reference {
        return format!("reference:{}", reference.reference_id());
    }
    if let Some(symbol_id) = &item.details.symbol_id {
        return format!("symbol:{symbol_id}");
    }
    if let Some(document_id) = &item.details.document_id {
        return format!("document:{document_id}");
    }
    if let Some(file_id) = &item.details.file_id {
        return format!("file:{file_id}:{}", item.details.qualified_name);
    }
    format!("path:{}:{}", item.details.path, item.details.qualified_name)
}

fn merge_reference_evidence(
    retained: &mut Option<ReferenceEvidence>,
    incoming: Option<ReferenceEvidence>,
) {
    let Some(incoming) = incoming else {
        return;
    };
    let Some(existing) = retained else {
        *retained = Some(incoming);
        return;
    };
    if existing.reference_id() == incoming.reference_id() {
        existing.details.represented_site_count = existing
            .represented_site_count()
            .max(incoming.represented_site_count());
    }
}

fn merge_graph_evidence(retained: &mut Option<GraphEvidence>, incoming: Option<GraphEvidence>) {
    let Some(incoming) = incoming else {
        return;
    };
    let Some(existing) = retained else {
        *retained = Some(incoming);
        return;
    };
    let same_edge = graph_evidence_identity(existing) == graph_evidence_identity(&incoming);
    let site_count = existing.site_count.max(incoming.site_count);
    if graph_evidence_order(&incoming, existing).is_lt() {
        *existing = incoming;
    }
    if same_edge {
        existing.site_count = site_count;
    }
}

fn graph_evidence_order(left: &GraphEvidence, right: &GraphEvidence) -> std::cmp::Ordering {
    left.depth
        .cmp(&right.depth)
        .then_with(|| left.from_symbol_id.cmp(&right.from_symbol_id))
        .then_with(|| left.to_symbol_id.cmp(&right.to_symbol_id))
        .then_with(|| left.edge_kind.cmp(&right.edge_kind))
        .then_with(|| left.provenance.cmp(&right.provenance))
        .then_with(|| right.confidence.total_cmp(&left.confidence))
}

fn graph_evidence_identity(evidence: &GraphEvidence) -> (&SymbolId, &SymbolId, &str, &str, u8) {
    (
        &evidence.from_symbol_id,
        &evidence.to_symbol_id,
        &evidence.edge_kind,
        &evidence.provenance,
        evidence.depth,
    )
}

#[cfg(test)]
pub(crate) fn evidence_fixture(
    path: &str,
    qualified_name: &str,
    reason: EvidenceReason,
) -> EvidenceItem {
    let generation_id = match GenerationId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa") {
        Ok(value) => value,
        Err(error) => panic!("fixture generation id is invalid: {error}"),
    };
    let mut item = evidence_base(generation_id, path, qualified_name);
    item.details.reasons.push(reason);
    item
}

#[cfg(test)]
pub(crate) struct SearchEvidenceFixture {
    pub(crate) file_id: Option<FileId>,
    pub(crate) symbol_id: Option<SymbolId>,
    pub(crate) language: SourceLanguage,
    pub(crate) document_kind: DocumentKind,
    pub(crate) fused_rank: Option<u16>,
    pub(crate) reciprocal_rank_score: Option<f64>,
}

#[cfg(test)]
pub(crate) fn enrich_search_evidence_fixture(
    mut item: EvidenceItem,
    input: SearchEvidenceFixture,
) -> EvidenceItem {
    item.details.file_id = input.file_id;
    item.details.symbol_id = input.symbol_id;
    item.details.language = Some(input.language);
    item.details.document_kind = Some(input.document_kind);
    item.details.fused_rank = input.fused_rank;
    item.details.reciprocal_rank_score = input.reciprocal_rank_score;
    item
}

#[cfg(test)]
pub(crate) struct ReferenceEvidenceFixture<'input> {
    pub(crate) reference_id: u64,
    pub(crate) path: &'input str,
    pub(crate) qualified_name: &'input str,
    pub(crate) owner_symbol_id: Option<SymbolId>,
    pub(crate) target_symbol_id: Option<SymbolId>,
    pub(crate) start_byte: u64,
    pub(crate) end_byte: u64,
    pub(crate) span_precision: ReferenceSpanPrecision,
    pub(crate) confidence: f32,
    pub(crate) provenance: &'input str,
    pub(crate) represented_site_count: u64,
}

#[cfg(test)]
pub(crate) fn reference_evidence_fixture(input: ReferenceEvidenceFixture<'_>) -> EvidenceItem {
    let mut item = evidence_fixture(
        input.path,
        input.qualified_name,
        reference_reason(input.span_precision),
    );
    item.details.symbol_id = input
        .target_symbol_id
        .as_ref()
        .or(input.owner_symbol_id.as_ref())
        .cloned();
    item.details.reference = Some(ReferenceEvidence {
        details: ReferenceEvidenceDetails {
            reference_id: input.reference_id,
            owner_symbol_id: input.owner_symbol_id,
            target_symbol_id: input.target_symbol_id,
            start_byte: input.start_byte,
            end_byte: input.end_byte,
            span_precision: input.span_precision,
            confidence: input.confidence,
            provenance: input.provenance.to_owned(),
            represented_site_count: input.represented_site_count,
        },
    });
    item
}

#[cfg(test)]
pub(crate) struct GraphEvidenceFixture<'input> {
    pub(crate) from_symbol_id: SymbolId,
    pub(crate) to_symbol_id: SymbolId,
    pub(crate) depth: u8,
    pub(crate) edge_kind: &'input str,
    pub(crate) confidence: f32,
    pub(crate) provenance: &'input str,
    pub(crate) site_count: u64,
}

#[cfg(test)]
pub(crate) fn graph_evidence_fixture(
    path: &str,
    qualified_name: &str,
    input: GraphEvidenceFixture<'_>,
) -> EvidenceItem {
    let mut item = evidence_fixture(path, qualified_name, EvidenceReason::Graph);
    item.details.symbol_id = Some(input.to_symbol_id.clone());
    item.details.graph = Some(GraphEvidence {
        from_symbol_id: input.from_symbol_id,
        to_symbol_id: input.to_symbol_id,
        depth: input.depth,
        edge_kind: input.edge_kind.to_owned(),
        confidence: input.confidence,
        provenance: input.provenance.to_owned(),
        site_count: input.site_count,
    });
    item
}

fn evidence_base(
    generation_id: GenerationId,
    path: impl Into<String>,
    qualified_name: impl Into<String>,
) -> EvidenceItem {
    EvidenceItem {
        details: EvidenceDetails {
            generation_id,
            file_id: None,
            symbol_id: None,
            document_id: None,
            path: path.into(),
            language: None,
            document_kind: None,
            qualified_name: qualified_name.into(),
            start_line: None,
            end_line: None,
            reasons: Vec::new(),
            fused_rank: None,
            reciprocal_rank_score: None,
            bm25_rank: None,
            bm25_score: None,
            bm25_components: Vec::new(),
            reference: None,
            graph: None,
            decision_relevance: None,
        },
    }
}

fn minimum_option<Value: Ord + Copy>(left: Option<Value>, right: Option<Value>) -> Option<Value> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left @ Some(_), None) => left,
        (None, right) => right,
    }
}

fn maximum_score(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left @ Some(_), None) => left,
        (None, right) => right,
    }
}

const fn reference_span_precision(
    precision: DatabaseReferenceSpanPrecision,
) -> ReferenceSpanPrecision {
    match precision {
        DatabaseReferenceSpanPrecision::Exact => ReferenceSpanPrecision::Exact,
        DatabaseReferenceSpanPrecision::CoarsePoint => ReferenceSpanPrecision::CoarsePoint,
        DatabaseReferenceSpanPrecision::CoarseOwner => ReferenceSpanPrecision::CoarseOwner,
    }
}

const fn reference_reason(precision: ReferenceSpanPrecision) -> EvidenceReason {
    match precision {
        ReferenceSpanPrecision::Exact => EvidenceReason::ExactReference,
        ReferenceSpanPrecision::CoarsePoint | ReferenceSpanPrecision::CoarseOwner => {
            EvidenceReason::CoarseReference
        }
    }
}

const fn search_component(component: LexicalComponent) -> SearchComponent {
    match component {
        LexicalComponent::QualifiedName => SearchComponent::QualifiedName,
        LexicalComponent::Code => SearchComponent::Code,
        LexicalComponent::NaturalText => SearchComponent::NaturalText,
    }
}
#[path = "model_packets.rs"]
mod packets;

pub use packets::{
    ContextAbstention, ContextPacket, DecisionRankEvidence, DecisionRankState, EditCandidate,
    EditCandidateBasis, EditCandidateSet, GenerationEvidence, RetrievalConfidence,
    ReviewAbstention, ReviewBudget, ReviewBudgetInput, ReviewPacket, ReviewRequest,
    ReviewRequestOptions, ReviewTruncation, WorkingTreeChangeKind, WorkingTreeEvidence,
    WorkingTreeEvidenceInput, WorkingTreeOverlay, WorkingTreeOverlayInput,
    WorkingTreeOverlayStatus,
};
pub(crate) use packets::{ContextPacketDetails, EditCandidateInput, ReviewPacketDetails};

fn validate_query(mut query: String) -> Result<String, RetrievalError> {
    let leading_whitespace = query.len() - query.trim_start().len();
    let trimmed_length = query.trim().len();
    if trimmed_length == 0 || trimmed_length > CONTEXT_QUERY_MAXIMUM_BYTES || query.contains('\0') {
        return Err(invalid("query"));
    }
    query.truncate(leading_whitespace + trimmed_length);
    query.drain(..leading_whitespace);
    Ok(query)
}

fn validate_anchor(anchor: &ContextAnchor) -> Result<(), RetrievalError> {
    let value = match anchor {
        ContextAnchor::ExactName(value) | ContextAnchor::ExactReference(value) => value,
        ContextAnchor::ExactPath(_) => return Ok(()),
    };
    if value.trim().is_empty() || value.len() > CONTEXT_ANCHOR_MAXIMUM_BYTES || value.contains('\0')
    {
        return Err(invalid("anchor"));
    }
    Ok(())
}

fn validate_lookup_text(value: &str, limit: u16) -> Result<(), RetrievalError> {
    if value.trim().is_empty() || value.len() > CONTEXT_ANCHOR_MAXIMUM_BYTES || value.contains('\0')
    {
        return Err(invalid("lookup"));
    }
    validate_limit(limit, MAX_EXACT_RESULTS, "exact_limit")
}

const fn validate_limit(
    value: u16,
    maximum: u16,
    field: &'static str,
) -> Result<(), RetrievalError> {
    if value == 0 || value > maximum {
        return Err(invalid(field));
    }
    Ok(())
}

fn validate_context_budget(input: &ContextBudgetInput) -> Result<(), RetrievalError> {
    validate_limit(input.candidate_limit, MAX_CANDIDATES, "candidate_limit")?;
    validate_limit(input.exact_limit, MAX_EXACT_RESULTS, "exact_limit")?;
    validate_packet_limits(input.evidence_limit, input.affected_test_limit)
}

fn validate_review_budget(input: &ReviewBudgetInput) -> Result<(), RetrievalError> {
    validate_limit(
        input.symbols_per_file,
        MAX_EXACT_RESULTS,
        "symbols_per_file",
    )?;
    validate_limit(input.root_limit, MAX_REVIEW_ROOTS, "root_limit")?;
    validate_packet_limits(input.evidence_limit, input.affected_test_limit)
}

const fn validate_packet_limits(
    evidence_limit: u16,
    affected_test_limit: u16,
) -> Result<(), RetrievalError> {
    if evidence_limit == 0 || evidence_limit > MAX_PACKET_EVIDENCE {
        return Err(invalid("evidence_limit"));
    }
    if affected_test_limit == 0 || affected_test_limit > MAX_AFFECTED_TESTS {
        return Err(invalid("affected_test_limit"));
    }
    Ok(())
}

fn validate_minimum_confidence(minimum: f32) -> Result<(), RetrievalError> {
    if minimum.is_finite() && (0.0..=1.0).contains(&minimum) {
        Ok(())
    } else {
        Err(invalid("minimum_confidence"))
    }
}

const fn invalid(field: &'static str) -> RetrievalError {
    RetrievalError::InvalidInput { field }
}
