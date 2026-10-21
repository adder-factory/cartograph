use std::{cmp::Ordering, collections::BTreeMap, collections::BTreeSet};

use cartograph_domain::{
    DocumentId, DocumentKind, FileId, GenerationId, NormalizedPath, SourceLanguage, SymbolId,
    SymbolKind,
};
use serde::Serialize;

use crate::{CONTEXT_ANCHOR_MAXIMUM_BYTES, RetrievalError};

const MAXIMUM_FUSED_RESULTS: u16 = 100;
const MAXIMUM_RERANK_MODEL_BYTES: usize = 256;
const RECIPROCAL_RANK_OFFSET: f64 = 60.0;

/// Retrieval policy requested by a caller before any channel work begins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Use only exact, lexical, and structural deterministic evidence.
    #[default]
    Deterministic,
    /// Fuse independently produced lexical and semantic candidate channels.
    Hybrid,
    /// Use hybrid retrieval when semantic evidence is ready, otherwise degrade explicitly.
    Auto,
}

/// Query-aware ordering policy applied after channel scores are retained unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalPreference {
    /// Preserve pure reciprocal-rank ordering.
    #[default]
    Neutral,
    /// Prefer production declarations over parameters, imports, fixtures, benches, and tests.
    ProductionDefinitions,
}

/// Caller-owned assessment of whether semantic evidence is safe to query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticReadiness {
    /// Semantic vectors and their model identity match the current generation.
    Ready,
    /// No semantic backend or model is configured.
    NotConfigured,
    /// The current generation has not been embedded.
    NotIndexed,
    /// Semantic evidence belongs to a non-current generation or model identity.
    Stale,
    /// The semantic channel could not produce a trustworthy result.
    Unavailable,
}

/// Independent retrieval channel that admitted a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalChannel {
    /// Deterministic BM25/lexical search.
    Lexical,
    /// Vector similarity search.
    Semantic,
}

/// Observable outcome of the optional cross-encoder stage over merged candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankState {
    /// Deterministic retrieval deliberately bypassed every model-backed stage.
    NotRequested,
    /// No reranker tier is configured for this project.
    NotConfigured,
    /// A configured reranker had no candidates to score.
    SkippedNoCandidates,
    /// Candidate metadata contained no bounded text safe to score.
    SkippedNoText,
    /// The configured reranker produced one complete finite score set.
    Applied,
    /// Configuration, endpoint, timeout, or response validation failed closed.
    Unavailable,
}

/// Query-free reranker provenance retained beside reciprocal-rank evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RerankReport {
    state: RerankState,
    model: Option<String>,
    reranked_documents: u16,
}

impl Default for RerankReport {
    fn default() -> Self {
        Self::not_configured()
    }
}

impl RerankReport {
    /// Record a caller policy that excludes model-backed reranking.
    #[must_use]
    pub const fn not_requested() -> Self {
        Self {
            state: RerankState::NotRequested,
            model: None,
            reranked_documents: 0,
        }
    }

    /// Record the absence of a project reranker tier.
    #[must_use]
    pub const fn not_configured() -> Self {
        Self {
            state: RerankState::NotConfigured,
            model: None,
            reranked_documents: 0,
        }
    }

    /// Record a configured model that had no candidates.
    /// # Errors
    ///
    /// Returns an error if `model` is empty, contains a NUL byte, or exceeds
    /// the bounded model-identifier length.
    pub fn skipped_no_candidates(model: impl Into<String>) -> Result<Self, RetrievalError> {
        Self::for_model(RerankState::SkippedNoCandidates, model, 0)
    }

    /// Record a configured model that received no non-empty bounded text.
    /// # Errors
    ///
    /// Returns an error if `model` is empty, contains a NUL byte, or exceeds
    /// the bounded model-identifier length.
    pub fn skipped_no_text(model: impl Into<String>) -> Result<Self, RetrievalError> {
        Self::for_model(RerankState::SkippedNoText, model, 0)
    }

    /// Record one successful complete rerank response.
    /// # Errors
    ///
    /// Returns an error if `model` is invalid or `reranked_documents` is zero
    /// or exceeds the maximum fusion window.
    pub fn applied(
        model: impl Into<String>,
        reranked_documents: u16,
    ) -> Result<Self, RetrievalError> {
        if reranked_documents == 0 || reranked_documents > MAXIMUM_FUSED_RESULTS {
            return Err(invalid("reranked_documents"));
        }
        Self::for_model(RerankState::Applied, model, reranked_documents)
    }

    /// Record a configured or malformed tier that could not be used.
    /// # Errors
    ///
    /// Returns an error if the optional model identifier is empty, contains a
    /// NUL byte, or exceeds its byte bound.
    pub fn unavailable(model: Option<String>) -> Result<Self, RetrievalError> {
        if model.as_deref().is_some_and(invalid_rerank_model) {
            return Err(invalid("rerank_model"));
        }
        Ok(Self {
            state: RerankState::Unavailable,
            model,
            reranked_documents: 0,
        })
    }

    /// Outcome state for this retrieval.
    #[must_use]
    pub const fn state(&self) -> RerankState {
        self.state
    }

    /// Configured model identity when one was validated.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Number of deduplicated documents scored by a complete response.
    #[must_use]
    pub const fn reranked_documents(&self) -> u16 {
        self.reranked_documents
    }

    fn for_model(
        state: RerankState,
        model: impl Into<String>,
        reranked_documents: u16,
    ) -> Result<Self, RetrievalError> {
        let model = model.into();
        if invalid_rerank_model(&model) {
            return Err(invalid("rerank_model"));
        }
        Ok(Self {
            state,
            model: Some(model),
            reranked_documents,
        })
    }
}

fn invalid_rerank_model(model: &str) -> bool {
    model.is_empty() || model.len() > MAXIMUM_RERANK_MODEL_BYTES || model.contains('\0')
}

/// Lexical document fields that contributed to a BM25 candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalComponent {
    /// Qualified declaration names contributed.
    QualifiedName,
    /// Indexed source-code identifiers contributed.
    Code,
    /// Documentation or other natural language contributed.
    NaturalText,
}

/// Actual channel combination used to assemble a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalExecution {
    /// No candidate channel could be used.
    Abstained,
    /// Lexical candidates only.
    Lexical,
    /// Semantic candidates only.
    Semantic,
    /// Lexical and semantic candidates were fused.
    Hybrid,
}

/// Explicit reason a requested multi-channel search degraded to one channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalFallback {
    /// Semantic readiness was not `ready`, so semantic candidates were not trusted.
    SemanticNotReady,
    /// Semantic readiness was `ready`, but no semantic channel completed.
    SemanticUnavailable,
    /// The semantic channel completed without candidates.
    SemanticEmpty,
    /// No lexical channel completed.
    LexicalUnavailable,
    /// The lexical channel completed without candidates.
    LexicalEmpty,
}

/// Why a compact hybrid packet has no usable evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalAbstention {
    /// Neither policy-eligible channel completed.
    NoUsableChannel,
    /// Eligible channels completed but produced no candidates.
    NoRelevantEvidence,
}

/// Stable document metadata shared by lexical and semantic channel candidates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RetrievalDocument {
    #[serde(flatten)]
    details: RetrievalDocumentDetails,
}

/// Stable document identity shared by lexical and semantic retrieval channels.
pub struct RetrievalDocumentInput {
    /// Stable document ID for this record.
    pub document_id: DocumentId,
    /// Stable generation ID for this record.
    pub generation_id: GenerationId,
    /// Project-relative path for this record.
    pub path: NormalizedPath,
    /// Language for this record.
    pub language: SourceLanguage,
    /// Document kind for this record.
    pub document_kind: DocumentKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RetrievalDocumentDetails {
    document_id: DocumentId,
    generation_id: GenerationId,
    file_id: Option<FileId>,
    symbol_id: Option<SymbolId>,
    path: NormalizedPath,
    language: SourceLanguage,
    document_kind: DocumentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<SymbolKind>,
    qualified_name: String,
}

impl RetrievalDocument {
    /// Create the minimum stable identity required for cross-channel fusion.
    #[must_use]
    pub fn new(input: RetrievalDocumentInput) -> Self {
        let RetrievalDocumentInput {
            document_id,
            generation_id,
            path,
            language,
            document_kind,
        } = input;
        Self {
            details: RetrievalDocumentDetails {
                document_id,
                generation_id,
                file_id: None,
                symbol_id: None,
                path,
                language,
                document_kind,
                symbol_kind: None,
                qualified_name: String::new(),
            },
        }
    }

    /// Attach an optional file identity.
    #[must_use]
    pub fn with_file_id(mut self, file_id: FileId) -> Self {
        self.details.file_id = Some(file_id);
        self
    }

    /// Attach an optional symbol identity.
    #[must_use]
    pub fn with_symbol_id(mut self, symbol_id: SymbolId) -> Self {
        self.details.symbol_id = Some(symbol_id);
        self
    }

    /// Attach the exact extracted symbol category used by result preference policy.
    #[must_use]
    pub const fn with_symbol_kind(mut self, symbol_kind: SymbolKind) -> Self {
        self.details.symbol_kind = Some(symbol_kind);
        self
    }

    /// Attach a bounded qualified name without exposing source or query text.
    /// # Errors
    ///
    /// Returns an error if `qualified_name` contains a NUL byte or exceeds the
    /// context-anchor byte limit.
    pub fn with_qualified_name(
        mut self,
        qualified_name: impl Into<String>,
    ) -> Result<Self, RetrievalError> {
        let qualified_name = qualified_name.into();
        if qualified_name.len() > CONTEXT_ANCHOR_MAXIMUM_BYTES || qualified_name.contains('\0') {
            return Err(invalid("qualified_name"));
        }
        self.details.qualified_name = qualified_name;
        Ok(self)
    }

    /// Stable logical search-document identity used for fusion.
    #[must_use]
    pub const fn document_id(&self) -> &DocumentId {
        &self.details.document_id
    }

    /// Immutable published generation that produced this candidate.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.details.generation_id
    }

    /// Canonical project-relative path.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.details.path
    }

    /// Typed source language retained from the indexed search document.
    #[must_use]
    pub const fn language(&self) -> SourceLanguage {
        self.details.language
    }

    /// Typed search-document role used by intent routing and evidence consumers.
    #[must_use]
    pub const fn document_kind(&self) -> DocumentKind {
        self.details.document_kind
    }

    /// Extracted symbol category when the search document is symbol-backed.
    #[must_use]
    pub const fn symbol_kind(&self) -> Option<SymbolKind> {
        self.details.symbol_kind
    }

    /// Optional current-generation file identity.
    #[must_use]
    pub const fn file_id(&self) -> Option<&FileId> {
        self.details.file_id.as_ref()
    }

    /// Optional current-generation symbol identity.
    #[must_use]
    pub const fn symbol_id(&self) -> Option<&SymbolId> {
        self.details.symbol_id.as_ref()
    }

    /// Qualified declaration name when the document is symbol-backed.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.details.qualified_name
    }
}

/// One already-ranked candidate from an independently completed channel.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChannelCandidate {
    document: RetrievalDocument,
    rank: u16,
    raw_score: f64,
    lexical_components: Vec<LexicalComponent>,
}

impl ChannelCandidate {
    /// Create a channel candidate with a rank inside the bounded fusion window.
    /// # Errors
    ///
    /// Returns an error if `rank` is outside the one-based fusion window or
    /// `raw_score` is not finite.
    pub fn new(
        document: RetrievalDocument,
        rank: u16,
        raw_score: f64,
    ) -> Result<Self, RetrievalError> {
        if rank == 0 || rank > MAXIMUM_FUSED_RESULTS {
            return Err(invalid("channel_rank"));
        }
        if !raw_score.is_finite() {
            return Err(invalid("channel_score"));
        }
        Ok(Self {
            document,
            rank,
            raw_score,
            lexical_components: Vec::new(),
        })
    }

    /// Retain the ordered BM25 fields that contributed to lexical matching.
    #[must_use]
    pub fn with_lexical_components(mut self, mut components: Vec<LexicalComponent>) -> Self {
        components.sort_unstable();
        components.dedup();
        self.lexical_components = components;
        self
    }

    /// Stable document metadata.
    #[must_use]
    pub const fn document(&self) -> &RetrievalDocument {
        &self.document
    }

    /// One-based raw rank within this channel.
    #[must_use]
    pub const fn rank(&self) -> u16 {
        self.rank
    }

    /// Native channel score retained for explanation, never compared across channels.
    #[must_use]
    pub const fn raw_score(&self) -> f64 {
        self.raw_score
    }

    /// Ordered BM25 components; empty for semantic candidates.
    #[must_use]
    pub fn lexical_components(&self) -> &[LexicalComponent] {
        &self.lexical_components
    }
}

/// One channel's complete bounded result, suitable for joining after concurrent work.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChannelResults {
    channel: RetrievalChannel,
    candidates: Vec<ChannelCandidate>,
    truncated: bool,
}

impl ChannelResults {
    /// Validate unique ranks/documents and normalize candidate order.
    /// # Errors
    ///
    /// Returns an error if candidates exceed the channel bound, contain a
    /// duplicate document/rank, or belong to an incompatible channel.
    pub fn new(
        channel: RetrievalChannel,
        mut candidates: Vec<ChannelCandidate>,
    ) -> Result<Self, RetrievalError> {
        validate_channel_candidates(channel, &candidates)?;
        candidates.sort_by(channel_candidate_order);
        Ok(Self {
            channel,
            candidates,
            truncated: false,
        })
    }

    /// Record upstream candidate truncation.
    #[must_use]
    pub const fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// Channel identity.
    #[must_use]
    pub const fn channel(&self) -> RetrievalChannel {
        self.channel
    }

    /// Candidates normalized by raw rank and stable identity.
    #[must_use]
    pub fn candidates(&self) -> &[ChannelCandidate] {
        &self.candidates
    }

    /// Whether the upstream channel omitted candidates.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Independently completed channels that can be attached in either completion order.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RetrievalChannels {
    lexical: Option<ChannelResults>,
    semantic: Option<ChannelResults>,
    rerank: RerankReport,
    rerank_scores: BTreeMap<DocumentId, f32>,
}

impl RetrievalChannels {
    /// Create an empty concurrent-channel join point.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lexical: None,
            semantic: None,
            rerank: RerankReport::not_configured(),
            rerank_scores: BTreeMap::new(),
        }
    }

    /// Attach one completed channel; duplicate completion fails closed.
    /// # Errors
    ///
    /// Returns an error if a result for the same lexical or semantic channel
    /// has already been attached.
    pub fn with_channel(mut self, results: ChannelResults) -> Result<Self, RetrievalError> {
        let target = match results.channel {
            RetrievalChannel::Lexical => &mut self.lexical,
            RetrievalChannel::Semantic => &mut self.semantic,
        };
        if target.is_some() {
            return Err(invalid("duplicate_channel"));
        }
        *target = Some(results);
        Ok(self)
    }

    /// Completed lexical channel, when available.
    #[must_use]
    pub const fn lexical(&self) -> Option<&ChannelResults> {
        self.lexical.as_ref()
    }

    /// Completed semantic channel, when available.
    #[must_use]
    pub const fn semantic(&self) -> Option<&ChannelResults> {
        self.semantic.as_ref()
    }

    /// Attach the exact optional reranker outcome observed for this query.
    #[must_use]
    pub fn with_rerank_report(mut self, report: RerankReport) -> Self {
        self.rerank = report;
        self.rerank_scores.clear();
        self
    }

    /// Attach validated cross-encoder scores without replacing either channel's evidence.
    ///
    /// A bounded shortlist may omit lower fused candidates. Unscored candidates retain their
    /// original ordering after scored candidates within the same retrieval preference tier.
    /// # Errors
    /// Rejects empty/oversized batches, duplicates, unknown documents, and scores outside [0, 1].
    pub fn with_fused_rerank(
        mut self,
        model: impl Into<String>,
        scores: Vec<(DocumentId, f32)>,
    ) -> Result<Self, RetrievalError> {
        let count = u16::try_from(scores.len()).map_err(|_| invalid("reranked_documents"))?;
        let report = RerankReport::applied(model, count)?;
        let known = self
            .lexical
            .iter()
            .chain(self.semantic.iter())
            .flat_map(ChannelResults::candidates)
            .map(|candidate| candidate.document().document_id())
            .collect::<BTreeSet<_>>();
        let mut validated = BTreeMap::new();
        for (document, score) in scores {
            if !score.is_finite()
                || !(0.0..=1.0).contains(&score)
                || !known.contains(&document)
                || validated.insert(document, score).is_some()
            {
                return Err(invalid("rerank_scores"));
            }
        }
        self.rerank = report;
        self.rerank_scores = validated;
        Ok(self)
    }

    /// Optional reranker outcome carried into fusion provenance.
    #[must_use]
    pub const fn rerank_report(&self) -> &RerankReport {
        &self.rerank
    }
}

/// Channel-independent fusion input that can be assembled in completion order.
#[derive(Clone, Debug, PartialEq)]
pub struct HybridSearchInput {
    mode: SearchMode,
    semantic_readiness: SemanticReadiness,
    result_limit: u16,
    preference: RetrievalPreference,
    channels: RetrievalChannels,
}

impl HybridSearchInput {
    /// Create a bounded fusion request before independently executing channels.
    /// # Errors
    ///
    /// Returns an error if `result_limit` is zero or exceeds the maximum fused
    /// result window.
    pub const fn new(
        mode: SearchMode,
        semantic_readiness: SemanticReadiness,
        result_limit: u16,
    ) -> Result<Self, RetrievalError> {
        if result_limit == 0 || result_limit > MAXIMUM_FUSED_RESULTS {
            return Err(invalid("result_limit"));
        }
        Ok(Self {
            mode,
            semantic_readiness,
            result_limit,
            preference: RetrievalPreference::Neutral,
            channels: RetrievalChannels::new(),
        })
    }

    /// Attach one independently completed channel; duplicate channels fail closed.
    /// # Errors
    ///
    /// Returns an error if a result for the same lexical or semantic channel
    /// has already been attached.
    pub fn with_channel(mut self, results: ChannelResults) -> Result<Self, RetrievalError> {
        self.channels = self.channels.with_channel(results)?;
        Ok(self)
    }

    /// Attach an already joined channel set produced by concurrent callers.
    #[must_use]
    pub fn with_channels(mut self, channels: RetrievalChannels) -> Self {
        self.channels = channels;
        self
    }

    /// Select a deterministic result-order preference without changing channel scores.
    #[must_use]
    pub const fn with_preference(mut self, preference: RetrievalPreference) -> Self {
        self.preference = preference;
        self
    }
}

/// Raw per-channel evidence retained beneath one reciprocal-rank result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChannelContribution {
    channel: RetrievalChannel,
    rank: u16,
    raw_score: f64,
    reciprocal_rank_score: f64,
    lexical_components: Vec<LexicalComponent>,
}

impl ChannelContribution {
    /// Source channel.
    #[must_use]
    pub const fn channel(&self) -> RetrievalChannel {
        self.channel
    }

    /// One-based raw rank in the source channel.
    #[must_use]
    pub const fn rank(&self) -> u16 {
        self.rank
    }

    /// Native channel score, meaningful only inside the source channel.
    #[must_use]
    pub const fn raw_score(&self) -> f64 {
        self.raw_score
    }

    /// This channel's additive reciprocal-rank contribution.
    #[must_use]
    pub const fn reciprocal_rank_score(&self) -> f64 {
        self.reciprocal_rank_score
    }

    /// Ordered BM25 components; empty for semantic contributions.
    #[must_use]
    pub fn lexical_components(&self) -> &[LexicalComponent] {
        &self.lexical_components
    }
}

/// One compact fused document with complete raw channel explanation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FusedSearchItem {
    document: RetrievalDocument,
    rank: u16,
    reciprocal_rank_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rerank_score: Option<f32>,
    contributions: Vec<ChannelContribution>,
}

impl FusedSearchItem {
    /// Stable document metadata without source bodies.
    #[must_use]
    pub const fn document(&self) -> &RetrievalDocument {
        &self.document
    }

    /// One-based rank after deterministic fusion and tie-breaking.
    #[must_use]
    pub const fn rank(&self) -> u16 {
        self.rank
    }

    /// Sum of per-channel reciprocal-rank contributions.
    #[must_use]
    pub const fn reciprocal_rank_score(&self) -> f64 {
        self.reciprocal_rank_score
    }

    /// Separate cross-encoder score, when this document was in the merged shortlist.
    #[must_use]
    pub const fn rerank_score(&self) -> Option<f32> {
        self.rerank_score
    }

    /// Contributions ordered lexical then semantic.
    #[must_use]
    pub fn contributions(&self) -> &[ChannelContribution] {
        &self.contributions
    }
}

/// Compact, serializable, query-free result of deterministic channel fusion.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HybridSearchPacket {
    #[serde(flatten)]
    details: HybridSearchDetails,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct HybridSearchDetails {
    generation_id: Option<GenerationId>,
    requested_mode: SearchMode,
    execution: RetrievalExecution,
    semantic_readiness: SemanticReadiness,
    preference: RetrievalPreference,
    rerank: RerankReport,
    fallback: Option<RetrievalFallback>,
    abstention: Option<RetrievalAbstention>,
    items: Vec<FusedSearchItem>,
    truncated: bool,
}

impl HybridSearchPacket {
    /// Generation shared by every admitted candidate.
    #[must_use]
    pub const fn generation_id(&self) -> Option<&GenerationId> {
        self.details.generation_id.as_ref()
    }

    /// Caller-requested retrieval policy.
    #[must_use]
    pub const fn requested_mode(&self) -> SearchMode {
        self.details.requested_mode
    }

    /// Channel combination actually used.
    #[must_use]
    pub const fn execution(&self) -> RetrievalExecution {
        self.details.execution
    }

    /// Semantic readiness considered by policy selection.
    #[must_use]
    pub const fn semantic_readiness(&self) -> SemanticReadiness {
        self.details.semantic_readiness
    }

    /// Query-aware result-order preference applied after fusion.
    #[must_use]
    pub const fn preference(&self) -> RetrievalPreference {
        self.details.preference
    }

    /// Optional semantic reranker outcome for this exact packet.
    #[must_use]
    pub const fn rerank_report(&self) -> &RerankReport {
        &self.details.rerank
    }

    /// Explicit single-channel degradation reason.
    #[must_use]
    pub const fn fallback(&self) -> Option<RetrievalFallback> {
        self.details.fallback
    }

    /// Explicit reason no results were admitted.
    #[must_use]
    pub const fn abstention(&self) -> Option<RetrievalAbstention> {
        self.details.abstention
    }

    /// Compact fused evidence ordered by score and stable identity.
    #[must_use]
    pub fn items(&self) -> &[FusedSearchItem] {
        &self.details.items
    }

    /// Whether an upstream channel or packet limit omitted candidates.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.details.truncated
    }
}

struct SelectedChannels<'input> {
    lexical: Option<&'input ChannelResults>,
    semantic: Option<&'input ChannelResults>,
}

struct FusionAccumulator {
    document: RetrievalDocument,
    contributions: Vec<ChannelContribution>,
}

/// Fuse already-ranked independent channels using reciprocal-rank fusion.
/// # Errors
///
/// Returns an error if selected channels disagree on generation identity,
/// contain invalid ranks, or cannot be assigned bounded fused ranks.
pub fn fuse_search(input: HybridSearchInput) -> Result<HybridSearchPacket, RetrievalError> {
    let HybridSearchInput {
        mode,
        semantic_readiness,
        result_limit,
        preference,
        channels,
    } = input;
    let selected = select_channels(mode, semantic_readiness, &channels);
    let execution = retrieval_execution(&selected);
    let fallback = retrieval_fallback(mode, semantic_readiness, &selected);
    let abstention = retrieval_abstention(&selected);
    let (generation_id, mut items) = fuse_selected_channels(&selected)?;
    if mode != SearchMode::Deterministic {
        for item in &mut items {
            item.rerank_score = channels
                .rerank_scores
                .get(item.document.document_id())
                .copied();
        }
    }
    items.sort_by(|left, right| fused_item_order(left, right, preference));
    assign_fused_ranks(&mut items)?;
    let packet_was_truncated = items.len() > usize::from(result_limit);
    items.truncate(usize::from(result_limit));
    let channel_was_truncated = selected.iter().any(ChannelResults::truncated);
    let rerank = if mode == SearchMode::Deterministic {
        RerankReport::not_requested()
    } else {
        channels.rerank
    };
    Ok(HybridSearchPacket {
        details: HybridSearchDetails {
            generation_id,
            requested_mode: mode,
            execution,
            semantic_readiness,
            preference,
            rerank,
            fallback,
            abstention,
            items,
            truncated: packet_was_truncated || channel_was_truncated,
        },
    })
}

impl SelectedChannels<'_> {
    fn iter(&self) -> impl Iterator<Item = &ChannelResults> {
        self.lexical.into_iter().chain(self.semantic)
    }

    fn completed_count(&self) -> usize {
        self.iter().count()
    }
}

fn select_channels(
    mode: SearchMode,
    semantic_readiness: SemanticReadiness,
    channels: &RetrievalChannels,
) -> SelectedChannels<'_> {
    let semantic_is_allowed =
        mode != SearchMode::Deterministic && semantic_readiness == SemanticReadiness::Ready;
    SelectedChannels {
        lexical: channels.lexical.as_ref(),
        semantic: semantic_is_allowed
            .then_some(channels.semantic.as_ref())
            .flatten(),
    }
}

fn retrieval_execution(selected: &SelectedChannels<'_>) -> RetrievalExecution {
    match (
        channel_has_candidates(selected.lexical),
        channel_has_candidates(selected.semantic),
    ) {
        (true, true) => RetrievalExecution::Hybrid,
        (true, false) => RetrievalExecution::Lexical,
        (false, true) => RetrievalExecution::Semantic,
        (false, false) => RetrievalExecution::Abstained,
    }
}

fn retrieval_abstention(selected: &SelectedChannels<'_>) -> Option<RetrievalAbstention> {
    if selected.completed_count() == 0 {
        Some(RetrievalAbstention::NoUsableChannel)
    } else if selected.iter().all(|channel| channel.candidates.is_empty()) {
        Some(RetrievalAbstention::NoRelevantEvidence)
    } else {
        None
    }
}

fn retrieval_fallback(
    mode: SearchMode,
    semantic_readiness: SemanticReadiness,
    selected: &SelectedChannels<'_>,
) -> Option<RetrievalFallback> {
    if mode == SearchMode::Deterministic {
        return None;
    }
    if semantic_readiness != SemanticReadiness::Ready {
        return Some(RetrievalFallback::SemanticNotReady);
    }
    fallback_for_ready_channels(selected)
}

fn fallback_for_ready_channels(selected: &SelectedChannels<'_>) -> Option<RetrievalFallback> {
    match (selected.lexical, selected.semantic) {
        (None, Some(_)) => Some(RetrievalFallback::LexicalUnavailable),
        (Some(_), None) => Some(RetrievalFallback::SemanticUnavailable),
        (Some(lexical), Some(semantic)) if lexical.candidates.is_empty() => {
            (!semantic.candidates.is_empty()).then_some(RetrievalFallback::LexicalEmpty)
        }
        (Some(lexical), Some(semantic)) if semantic.candidates.is_empty() => {
            (!lexical.candidates.is_empty()).then_some(RetrievalFallback::SemanticEmpty)
        }
        (None, None) | (Some(_), Some(_)) => None,
    }
}

fn channel_has_candidates(channel: Option<&ChannelResults>) -> bool {
    channel.is_some_and(|results| !results.candidates.is_empty())
}

fn fuse_selected_channels(
    selected: &SelectedChannels<'_>,
) -> Result<(Option<GenerationId>, Vec<FusedSearchItem>), RetrievalError> {
    let mut generation_id = None;
    let mut fused = BTreeMap::<DocumentId, FusionAccumulator>::new();
    for results in selected.iter() {
        for candidate in &results.candidates {
            validate_generation(&mut generation_id, candidate.document.generation_id())?;
            insert_contribution(&mut fused, results.channel, candidate)?;
        }
    }
    let items = fused.into_values().map(fused_item).collect();
    Ok((generation_id, items))
}

fn validate_generation(
    generation_id: &mut Option<GenerationId>,
    candidate: &GenerationId,
) -> Result<(), RetrievalError> {
    if generation_id
        .as_ref()
        .is_some_and(|generation| generation != candidate)
    {
        return Err(invalid("mixed_generation"));
    }
    if generation_id.is_none() {
        *generation_id = Some(candidate.clone());
    }
    Ok(())
}

fn insert_contribution(
    fused: &mut BTreeMap<DocumentId, FusionAccumulator>,
    channel: RetrievalChannel,
    candidate: &ChannelCandidate,
) -> Result<(), RetrievalError> {
    let document_id = candidate.document.document_id().clone();
    let contribution = channel_contribution(channel, candidate);
    if let Some(existing) = fused.get_mut(&document_id) {
        if existing.document != candidate.document {
            return Err(invalid("channel_document"));
        }
        existing.contributions.push(contribution);
        return Ok(());
    }
    fused.insert(
        document_id,
        FusionAccumulator {
            document: candidate.document.clone(),
            contributions: vec![contribution],
        },
    );
    Ok(())
}

fn channel_contribution(
    channel: RetrievalChannel,
    candidate: &ChannelCandidate,
) -> ChannelContribution {
    ChannelContribution {
        channel,
        rank: candidate.rank,
        raw_score: candidate.raw_score,
        reciprocal_rank_score: reciprocal_rank(candidate.rank),
        lexical_components: candidate.lexical_components.clone(),
    }
}

fn reciprocal_rank(rank: u16) -> f64 {
    1.0 / (RECIPROCAL_RANK_OFFSET + f64::from(rank))
}

fn fused_item(mut accumulator: FusionAccumulator) -> FusedSearchItem {
    accumulator
        .contributions
        .sort_by_key(|contribution| contribution.channel);
    let reciprocal_rank_score = accumulator
        .contributions
        .iter()
        .map(|contribution| contribution.reciprocal_rank_score)
        .sum();
    FusedSearchItem {
        document: accumulator.document,
        rank: 0,
        reciprocal_rank_score,
        rerank_score: None,
        contributions: accumulator.contributions,
    }
}

fn fused_item_order(
    left: &FusedSearchItem,
    right: &FusedSearchItem,
    preference: RetrievalPreference,
) -> Ordering {
    preference_tier(left, preference)
        .cmp(&preference_tier(right, preference))
        .then_with(|| match (left.rerank_score, right.rerank_score) {
            (Some(left), Some(right)) => right.total_cmp(&left),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| {
            right
                .reciprocal_rank_score
                .total_cmp(&left.reciprocal_rank_score)
        })
        .then_with(|| best_raw_rank(left).cmp(&best_raw_rank(right)))
        .then_with(|| left.document.path().cmp(right.document.path()))
        .then_with(|| {
            left.document
                .qualified_name()
                .cmp(right.document.qualified_name())
        })
        .then_with(|| {
            left.document
                .document_id()
                .cmp(right.document.document_id())
        })
}

fn preference_tier(item: &FusedSearchItem, preference: RetrievalPreference) -> u8 {
    if preference == RetrievalPreference::Neutral {
        return 0;
    }
    let document = item.document();
    if document.document_kind() == DocumentKind::Test
        || cartograph_domain::is_test_source_path(document.path().as_str())
    {
        return 3;
    }
    if is_auxiliary_path(document.path().as_str()) {
        return 2;
    }
    match document.symbol_kind() {
        Some(SymbolKind::Parameter | SymbolKind::Import) => 2,
        Some(
            SymbolKind::Property
            | SymbolKind::Field
            | SymbolKind::Variable
            | SymbolKind::EnumMember
            | SymbolKind::Export,
        ) => 1,
        Some(
            SymbolKind::File
            | SymbolKind::Module
            | SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Union
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Protocol
            | SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::Constant
            | SymbolKind::Enum
            | SymbolKind::TypeAlias
            | SymbolKind::Namespace
            | SymbolKind::Route
            | SymbolKind::Component
            | SymbolKind::Table
            | SymbolKind::Resource,
        )
        | None => 0,
    }
}

fn is_auxiliary_path(path: &str) -> bool {
    path.split('/').any(|segment| {
        matches!(
            segment,
            "bench"
                | "benches"
                | "benchmark"
                | "benchmarks"
                | "example"
                | "examples"
                | "fixture"
                | "fixtures"
        )
    })
}

fn best_raw_rank(item: &FusedSearchItem) -> u16 {
    item.contributions
        .iter()
        .map(|contribution| contribution.rank)
        .min()
        .unwrap_or(u16::MAX)
}

fn assign_fused_ranks(items: &mut [FusedSearchItem]) -> Result<(), RetrievalError> {
    for (index, item) in items.iter_mut().enumerate() {
        item.rank =
            u16::try_from(index.saturating_add(1)).map_err(|_| invalid("fused_result_count"))?;
    }
    Ok(())
}

fn validate_channel_candidates(
    channel: RetrievalChannel,
    candidates: &[ChannelCandidate],
) -> Result<(), RetrievalError> {
    let mut ranks = BTreeSet::new();
    let mut documents = BTreeSet::new();
    for candidate in candidates {
        if !ranks.insert(candidate.rank) {
            return Err(invalid("duplicate_channel_rank"));
        }
        if !documents.insert(candidate.document.document_id().clone()) {
            return Err(invalid("duplicate_channel_document"));
        }
        if channel == RetrievalChannel::Semantic && !candidate.lexical_components.is_empty() {
            return Err(invalid("semantic_components"));
        }
    }
    Ok(())
}

fn channel_candidate_order(left: &ChannelCandidate, right: &ChannelCandidate) -> Ordering {
    left.rank
        .cmp(&right.rank)
        .then_with(|| left.document.path().cmp(right.document.path()))
        .then_with(|| {
            left.document
                .document_id()
                .cmp(right.document.document_id())
        })
}

const fn invalid(field: &'static str) -> RetrievalError {
    RetrievalError::InvalidInput { field }
}
