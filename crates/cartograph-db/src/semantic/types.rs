use std::{collections::BTreeSet, fmt, time::Duration};

use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, FileId, GenerationId, ModelId, ProjectId, SymbolId,
    SymbolKind,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAXIMUM_MODEL_DIMENSION: usize = 2_000;
const MAXIMUM_PROVIDER_BYTES: usize = 128;
const MAXIMUM_MODEL_NAME_BYTES: usize = 256;
const MAXIMUM_EMBEDDING_BATCH: usize = 128;
const MAXIMUM_BATCH_VECTOR_VALUES: usize = MAXIMUM_EMBEDDING_BATCH * MAXIMUM_MODEL_DIMENSION;
const MAXIMUM_PENDING_DOCUMENTS: u16 = 128;
pub(crate) const MAXIMUM_PENDING_BYTES: u64 = 16 * 1_024 * 1_024;
const MAXIMUM_VECTOR_RESULTS: u16 = 100;
const MAXIMUM_SIMILAR_SYMBOL_RESULTS: u16 = 50;
const MAXIMUM_STATEMENT_TIMEOUT: Duration = Duration::from_mins(30);

/// Whether vectors are retained as emitted or must already have unit L2 norm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingNormalization {
    /// Preserve any finite nonzero vector magnitude.
    None,
    /// Require unit L2 norm within the database's fixed tolerance.
    L2,
}

impl EmbeddingNormalization {}

/// Durable lifecycle of one model registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModelState {
    /// New current-generation vectors may be written and queried.
    Active,
    /// Historical vectors remain auditable but new writes/search are refused.
    Retired,
}

impl EmbeddingModelState {
    /// Whether the model has passed the audited active-to-retired transition.
    #[must_use]
    pub const fn is_retired(self) -> bool {
        matches!(self, Self::Retired)
    }
}

/// Immutable identity and vector contract proposed for the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingModelRegistration {
    model_id: ModelId,
    fingerprint: ContentDigest,
    provider: String,
    model_name: String,
    dimension: u16,
    normalization: EmbeddingNormalization,
}

/// Unvalidated model-registry fields consumed atomically by registration.
pub struct EmbeddingModelRegistrationInput {
    /// Stable model UUID.
    pub model_id: ModelId,
    /// Endpoint/model fingerprint without secret endpoint text.
    pub fingerprint: ContentDigest,
    /// Non-secret provider family.
    pub provider: String,
    /// Exact non-secret model name.
    pub model_name: String,
    /// Vector dimension promised by the backend.
    pub dimension: u16,
    /// Stored-vector normalization contract.
    pub normalization: EmbeddingNormalization,
}

impl EmbeddingModelRegistration {
    /// Validate non-secret model metadata and the native vector dimension ceiling.
    /// # Errors
    ///
    /// Returns an error if provider/model text is empty, oversized, or
    /// NUL-containing, or `dimension` is zero/above the vector ceiling.
    pub fn new(input: EmbeddingModelRegistrationInput) -> Result<Self, SemanticStorageError> {
        let provider = bounded_text(input.provider, "provider", MAXIMUM_PROVIDER_BYTES)?;
        let model_name = bounded_text(input.model_name, "model_name", MAXIMUM_MODEL_NAME_BYTES)?;
        if input.dimension == 0 || usize::from(input.dimension) > MAXIMUM_MODEL_DIMENSION {
            return Err(invalid("dimension"));
        }
        Ok(Self {
            model_id: input.model_id,
            fingerprint: input.fingerprint,
            provider,
            model_name,
            dimension: input.dimension,
            normalization: input.normalization,
        })
    }

    /// Exact selector callers retain after registration.
    #[must_use]
    pub fn selector(&self) -> EmbeddingModelSelector {
        EmbeddingModelSelector {
            model_id: self.model_id.clone(),
            fingerprint: self.fingerprint.clone(),
            dimension: self.dimension,
        }
    }

    /// Stable model UUID.
    #[must_use]
    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// Endpoint/model fingerprint without endpoint or credential text.
    #[must_use]
    pub const fn fingerprint(&self) -> &ContentDigest {
        &self.fingerprint
    }

    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    pub(crate) fn model_name(&self) -> &str {
        &self.model_name
    }

    pub(crate) const fn normalization(&self) -> EmbeddingNormalization {
        self.normalization
    }
}

/// Exact model identity, fingerprint, and dimension expected by one operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingModelSelector {
    pub(crate) model_id: ModelId,
    pub(crate) fingerprint: ContentDigest,
    pub(crate) dimension: u16,
}

impl EmbeddingModelSelector {
    /// Stable model UUID.
    #[must_use]
    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// Expected model fingerprint.
    #[must_use]
    pub const fn fingerprint(&self) -> &ContentDigest {
        &self.fingerprint
    }

    /// Expected vector dimension.
    #[must_use]
    pub const fn dimension(&self) -> u16 {
        self.dimension
    }
}

/// Registry row returned without endpoint, credential, or project path data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredEmbeddingModel {
    pub(crate) selector: EmbeddingModelSelector,
    pub(crate) provider: String,
    pub(crate) model_name: String,
    pub(crate) normalization: EmbeddingNormalization,
    pub(crate) state: EmbeddingModelState,
}

impl RegisteredEmbeddingModel {
    /// Exact selector for subsequent model-scoped operations.
    #[must_use]
    pub const fn selector(&self) -> &EmbeddingModelSelector {
        &self.selector
    }

    /// Durable model lifecycle state.
    #[must_use]
    pub const fn state(&self) -> EmbeddingModelState {
        self.state
    }

    /// Non-secret provider family, such as `openai-compatible`.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Exact non-secret model name sent to the embedding backend.
    #[must_use]
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Stored-vector normalization contract.
    #[must_use]
    pub const fn normalization(&self) -> EmbeddingNormalization {
        self.normalization
    }
}

/// Verified reader state for one exact registered vector model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingIndexStatus {
    pub(crate) model_id: ModelId,
    pub(crate) index_name: String,
    pub(crate) algorithm: &'static str,
    pub(crate) ready: bool,
}

impl EmbeddingIndexStatus {
    /// Actual search algorithm provided by this index implementation.
    #[must_use]
    pub const fn algorithm(&self) -> &'static str {
        self.algorithm
    }
    /// Exact registry model guarded by this index.
    #[must_use]
    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// Deterministic native reader identifier for this model.
    #[must_use]
    pub fn index_name(&self) -> &str {
        &self.index_name
    }

    /// Whether the model-specific vector reader is ready.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.ready
    }
}

/// Generation-bound keyset cursor for deterministic pending-document paging.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingPageCursor {
    pub(crate) generation_id: GenerationId,
    pub(crate) after_row_id: u64,
}

impl EmbeddingPageCursor {
    /// Generation this cursor is valid for.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Last consumed native document ordinal.
    #[must_use]
    pub const fn after_row_id(&self) -> u64 {
        self.after_row_id
    }
}

/// Bounded request for current documents that lack one exact model embedding.
pub struct PendingEmbeddingPageRequest {
    pub(crate) project_id: ProjectId,
    pub(crate) model: EmbeddingModelSelector,
    pub(crate) maximum_documents: u16,
    pub(crate) maximum_source_bytes: u64,
    pub(crate) statement_timeout: Duration,
    pub(crate) cursor: Option<EmbeddingPageCursor>,
}

/// Unvalidated paging fields consumed atomically by a pending-document request.
pub struct PendingEmbeddingPageInput {
    /// Project whose current generation is paged.
    pub project_id: ProjectId,
    /// Exact model whose missing vectors are selected.
    pub model: EmbeddingModelSelector,
    /// Maximum documents returned by one page.
    pub maximum_documents: u16,
    /// Maximum retained source text returned by one page.
    pub maximum_source_bytes: u64,
    /// Native operation deadline.
    pub statement_timeout: Duration,
}

impl PendingEmbeddingPageRequest {
    /// Validate page cardinality, retained source bytes, and database deadline.
    /// # Errors
    ///
    /// Returns an error if document/retained-byte bounds or the statement
    /// deadline are zero or exceed their semantic paging maxima.
    pub fn new(input: PendingEmbeddingPageInput) -> Result<Self, SemanticStorageError> {
        if input.maximum_documents == 0 || input.maximum_documents > MAXIMUM_PENDING_DOCUMENTS {
            return Err(invalid("maximum_documents"));
        }
        if input.maximum_source_bytes == 0 || input.maximum_source_bytes > MAXIMUM_PENDING_BYTES {
            return Err(invalid("maximum_source_bytes"));
        }
        validate_timeout(input.statement_timeout)?;
        Ok(Self {
            project_id: input.project_id,
            model: input.model,
            maximum_documents: input.maximum_documents,
            maximum_source_bytes: input.maximum_source_bytes,
            statement_timeout: input.statement_timeout,
            cursor: None,
        })
    }

    /// Resume only the exact generation named by a prior page.
    /// # Errors
    ///
    /// Returns an error if the cursor's last row identity is zero.
    pub fn with_cursor(
        mut self,
        cursor: EmbeddingPageCursor,
    ) -> Result<Self, SemanticStorageError> {
        if cursor.after_row_id == 0 {
            return Err(invalid("cursor"));
        }
        self.cursor = Some(cursor);
        Ok(self)
    }
}

/// One exact embedding input and its digest from a current search document.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct PendingEmbeddingDocument {
    pub(crate) row_id: u64,
    pub(crate) document_id: DocumentId,
    pub(crate) path: String,
    pub(crate) text: String,
    pub(crate) source_digest: ContentDigest,
}

impl PendingEmbeddingDocument {
    /// Stable logical document identity.
    #[must_use]
    pub const fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    /// Project-normalized source path retained for progress and diagnostics.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Exact text that must be sent to the embedding endpoint.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Domain-separated digest revalidated during the batch write.
    #[must_use]
    pub const fn source_digest(&self) -> &ContentDigest {
        &self.source_digest
    }
}

impl fmt::Debug for PendingEmbeddingDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingEmbeddingDocument")
            .field("row_id", &self.row_id)
            .field("document_id", &self.document_id)
            .field("path", &self.path)
            .field("text_bytes", &self.text.len())
            .field("source_digest", &self.source_digest)
            .finish()
    }
}

/// One bounded page from a single current generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingEmbeddingPage {
    pub(crate) generation_id: GenerationId,
    pub(crate) documents: Vec<PendingEmbeddingDocument>,
    pub(crate) next_cursor: Option<EmbeddingPageCursor>,
}

impl PendingEmbeddingPage {
    /// Current generation held for this page.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Deterministically ordered embedding inputs.
    #[must_use]
    pub fn documents(&self) -> &[PendingEmbeddingDocument] {
        &self.documents
    }

    /// Cursor for an omitted suffix, absent when the sweep is complete.
    #[must_use]
    pub const fn next_cursor(&self) -> Option<&EmbeddingPageCursor> {
        self.next_cursor.as_ref()
    }

    /// Consume the page without cloning source text for the embedding pipeline.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        GenerationId,
        Vec<PendingEmbeddingDocument>,
        Option<EmbeddingPageCursor>,
    ) {
        (self.generation_id, self.documents, self.next_cursor)
    }
}

/// One vector write paired with the exact source digest it represents.
#[derive(Clone, PartialEq)]
pub struct EmbeddingUpsertRow {
    pub(crate) document_id: DocumentId,
    pub(crate) source_digest: ContentDigest,
    pub(crate) vector: Vec<f32>,
}

impl EmbeddingUpsertRow {
    /// Validate one finite, nonzero, dimension-bounded vector.
    /// # Errors
    ///
    /// Returns an error if the vector is empty, oversized, all-zero, or
    /// contains a non-finite component.
    pub fn new(
        document_id: DocumentId,
        source_digest: ContentDigest,
        vector: Vec<f32>,
    ) -> Result<Self, SemanticStorageError> {
        validate_vector(&vector)?;
        Ok(Self {
            document_id,
            source_digest,
            vector,
        })
    }
}

impl fmt::Debug for EmbeddingUpsertRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddingUpsertRow")
            .field("document_id", &self.document_id)
            .field("source_digest", &self.source_digest)
            .field("dimension", &self.vector.len())
            .finish()
    }
}

/// Atomic bounded batch for one current generation and exact model.
pub struct EmbeddingBatchUpsertRequest {
    pub(crate) project_id: ProjectId,
    pub(crate) generation_id: GenerationId,
    pub(crate) model: EmbeddingModelSelector,
    pub(crate) rows: Vec<EmbeddingUpsertRow>,
    pub(crate) statement_timeout: Duration,
}

/// Unvalidated atomic embedding-batch fields.
pub struct EmbeddingBatchUpsertInput {
    /// Project whose current generation is written.
    pub project_id: ProjectId,
    /// Exact current generation expected by every row.
    pub generation_id: GenerationId,
    /// Exact registered model receiving the vectors.
    pub model: EmbeddingModelSelector,
    /// Source-digest-bound vectors to sort and validate.
    pub rows: Vec<EmbeddingUpsertRow>,
    /// Native operation deadline.
    pub statement_timeout: Duration,
}

impl EmbeddingBatchUpsertRequest {
    /// Validate batch count, distinct identities, vector dimensions, memory, and deadline.
    /// # Errors
    ///
    /// Returns an error if the batch is empty/oversized, has duplicate
    /// documents, wrong dimensions/too many values, or an invalid deadline.
    pub fn new(mut input: EmbeddingBatchUpsertInput) -> Result<Self, SemanticStorageError> {
        let rows = &mut input.rows;
        if rows.is_empty() || rows.len() > MAXIMUM_EMBEDDING_BATCH {
            return Err(invalid("embedding_batch"));
        }
        validate_timeout(input.statement_timeout)?;
        rows.sort_by(|left, right| left.document_id.cmp(&right.document_id));
        let mut identities = BTreeSet::new();
        let mut values = 0_usize;
        for row in rows.iter() {
            if !identities.insert(row.document_id.clone())
                || row.vector.len() != usize::from(input.model.dimension)
            {
                return Err(invalid("embedding_batch"));
            }
            values = values
                .checked_add(row.vector.len())
                .ok_or_else(|| invalid("embedding_batch"))?;
        }
        if values > MAXIMUM_BATCH_VECTOR_VALUES {
            return Err(invalid("embedding_batch"));
        }
        Ok(Self {
            project_id: input.project_id,
            generation_id: input.generation_id,
            model: input.model,
            rows: input.rows,
            statement_timeout: input.statement_timeout,
        })
    }
}

/// Idempotent batch outcome after one committed transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingBatchUpsertReport {
    pub(crate) requested: u16,
    pub(crate) written: u16,
}

impl EmbeddingBatchUpsertReport {
    /// Rows admitted to the atomic batch.
    #[must_use]
    pub const fn requested(self) -> u16 {
        self.requested
    }

    /// Rows inserted or changed.
    #[must_use]
    pub const fn written(self) -> u16 {
        self.written
    }

    /// Exact idempotent no-op count.
    #[must_use]
    pub const fn unchanged(self) -> u16 {
        self.requested.saturating_sub(self.written)
    }
}

/// Semantic retrieval readiness state with explicit fail-closed reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticReadinessState {
    /// Fingerprint, dimension, coverage, vector reader, and query probe all passed.
    Ready,
    /// The requested model UUID is not registered.
    ModelMissing,
    /// Registered fingerprint or dimension differs from the caller's selector.
    ModelMismatch,
    /// The exact model completed its audited retirement transition.
    ModelRetired,
    /// The project has no current structural/search generation.
    NoCurrentGeneration,
    /// The current generation contains no searchable documents.
    NoDocuments,
    /// One or more current documents lack this model's vector.
    CoverageIncomplete,
    /// The expected model-specific vector reader is absent, invalid, or malformed.
    IndexUnavailable,
    /// A bounded live nearest-neighbor probe did not succeed.
    QueryProbeFailed,
}

/// Bounded readiness request for one project/model pair.
pub struct SemanticReadinessRequest {
    pub(crate) project_id: ProjectId,
    pub(crate) model: EmbeddingModelSelector,
    pub(crate) statement_timeout: Duration,
}

impl SemanticReadinessRequest {
    /// Bind an exact project/model pair to a database deadline.
    /// # Errors
    ///
    /// Returns an error if `statement_timeout` is zero or exceeds the semantic
    /// operation deadline maximum.
    pub fn new(
        project_id: ProjectId,
        model: EmbeddingModelSelector,
        statement_timeout: Duration,
    ) -> Result<Self, SemanticStorageError> {
        validate_timeout(statement_timeout)?;
        Ok(Self {
            project_id,
            model,
            statement_timeout,
        })
    }
}

/// Exact coverage and live-index evidence used to gate semantic retrieval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SemanticReadinessReport {
    pub(crate) model_id: ModelId,
    pub(crate) generation_id: Option<GenerationId>,
    pub(crate) documents: u64,
    pub(crate) embedded: u64,
    pub(crate) index_ready: bool,
    pub(crate) query_probe_ready: bool,
    pub(crate) state: SemanticReadinessState,
}

impl SemanticReadinessReport {
    /// Exact model evaluated.
    #[must_use]
    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// Current generation evaluated, absent before first publication.
    #[must_use]
    pub const fn generation_id(&self) -> Option<&GenerationId> {
        self.generation_id.as_ref()
    }

    /// Terminal readiness state.
    #[must_use]
    pub const fn state(&self) -> SemanticReadinessState {
        self.state
    }

    /// Current-generation document count.
    #[must_use]
    pub const fn documents(&self) -> u64 {
        self.documents
    }

    /// Current-generation matching-model vector count.
    #[must_use]
    pub const fn embedded(&self) -> u64 {
        self.embedded
    }

    /// Whether the model-specific vector reader check passed.
    #[must_use]
    pub const fn index_ready(&self) -> bool {
        self.index_ready
    }

    /// Whether a live nearest-neighbor probe executed successfully.
    #[must_use]
    pub const fn query_probe_ready(&self) -> bool {
        self.query_probe_ready
    }

    /// Whether every required live proof passed.
    #[must_use]
    pub const fn ready(&self) -> bool {
        matches!(self.state, SemanticReadinessState::Ready)
    }
}

/// Bounded top-K query gated by semantic readiness.
pub struct VectorSearchRequest {
    pub(crate) project_id: ProjectId,
    pub(crate) expected_generation_id: GenerationId,
    pub(crate) model: EmbeddingModelSelector,
    pub(crate) vector: Vec<f32>,
    pub(crate) limit: u16,
    pub(crate) statement_timeout: Duration,
}

/// Unvalidated semantic nearest-neighbor query fields.
pub struct VectorSearchInput {
    /// Project whose current generation is searched.
    pub project_id: ProjectId,
    /// Publication identity every semantic stage must observe.
    pub expected_generation_id: GenerationId,
    /// Exact registered model expected by the vector.
    pub model: EmbeddingModelSelector,
    /// Finite nonzero query vector.
    pub vector: Vec<f32>,
    /// Maximum current-generation hits.
    pub limit: u16,
    /// Native operation deadline.
    pub statement_timeout: Duration,
}

impl VectorSearchRequest {
    /// Validate query vector, model dimension, result cap, and database deadline.
    /// # Errors
    ///
    /// Returns an error if the vector is invalid/wrong-dimensional, result
    /// limit is out of bounds, or the statement deadline is invalid.
    pub fn new(input: VectorSearchInput) -> Result<Self, SemanticStorageError> {
        validate_vector(&input.vector)?;
        if input.vector.len() != usize::from(input.model.dimension)
            || input.limit == 0
            || input.limit > MAXIMUM_VECTOR_RESULTS
        {
            return Err(invalid("vector_search"));
        }
        validate_timeout(input.statement_timeout)?;
        Ok(Self {
            project_id: input.project_id,
            expected_generation_id: input.expected_generation_id,
            model: input.model,
            vector: input.vector,
            limit: input.limit,
            statement_timeout: input.statement_timeout,
        })
    }
}

/// One current-generation nearest-neighbor result with exact provenance.
#[derive(Clone, PartialEq, Serialize)]
pub struct VectorSearchHit {
    pub(crate) generation_id: GenerationId,
    pub(crate) document_id: DocumentId,
    pub(crate) file_id: Option<FileId>,
    pub(crate) symbol_id: Option<SymbolId>,
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) document_kind: DocumentKind,
    pub(crate) qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) symbol_kind: Option<SymbolKind>,
    pub(crate) distance: f64,
    #[serde(skip)]
    pub(crate) rerank_text: Option<String>,
}

impl fmt::Debug for VectorSearchHit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VectorSearchHit")
            .field("generation_id", &self.generation_id)
            .field("document_id", &self.document_id)
            .field("file_id", &self.file_id)
            .field("symbol_id", &self.symbol_id)
            .field("path", &self.path)
            .field("language", &self.language)
            .field("document_kind", &self.document_kind)
            .field("qualified_name", &self.qualified_name)
            .field("symbol_kind", &self.symbol_kind)
            .field("distance", &self.distance)
            .field(
                "rerank_text_bytes",
                &self.rerank_text.as_ref().map(String::len),
            )
            .finish()
    }
}

/// Unvalidated stored-vector neighbor query for one exact current symbol.
pub struct SimilarSymbolsInput {
    /// Project whose current generation is searched.
    pub project_id: ProjectId,
    /// Publication identity every semantic stage must observe.
    pub expected_generation_id: GenerationId,
    /// Exact symbol whose stored embedding is used as the query vector.
    pub source_symbol_id: SymbolId,
    /// Optional active model choice; omitted only when the source has one active model.
    pub model_id: Option<ModelId>,
    /// Maximum symbol neighbors.
    pub limit: u16,
    /// Minimum cosine similarity in the inclusive 0..1 range.
    pub minimum_score: f64,
    /// Restrict candidates to the source symbol's indexed language.
    pub same_language: bool,
    /// Native operation deadline.
    pub statement_timeout: Duration,
}

/// Validated current-generation stored-vector neighbor request.
pub struct SimilarSymbolsRequest {
    pub(crate) project_id: ProjectId,
    pub(crate) expected_generation_id: GenerationId,
    pub(crate) source_symbol_id: SymbolId,
    pub(crate) model_id: Option<ModelId>,
    pub(crate) limit: u16,
    pub(crate) minimum_score: f64,
    pub(crate) same_language: bool,
    pub(crate) statement_timeout: Duration,
}

impl SimilarSymbolsRequest {
    /// Validate score, result, and deadline bounds before database work.
    /// # Errors
    ///
    /// Returns an error if result/score/deadline bounds are invalid or the
    /// minimum score is non-finite/outside zero to one.
    pub fn new(input: SimilarSymbolsInput) -> Result<Self, SemanticStorageError> {
        if input.limit == 0
            || input.limit > MAXIMUM_SIMILAR_SYMBOL_RESULTS
            || !input.minimum_score.is_finite()
            || !(0.0..=1.0).contains(&input.minimum_score)
        {
            return Err(invalid("similar_symbols"));
        }
        validate_timeout(input.statement_timeout)?;
        Ok(Self {
            project_id: input.project_id,
            expected_generation_id: input.expected_generation_id,
            source_symbol_id: input.source_symbol_id,
            model_id: input.model_id,
            limit: input.limit,
            minimum_score: input.minimum_score,
            same_language: input.same_language,
            statement_timeout: input.statement_timeout,
        })
    }
}

/// One symbol neighbor with exact model and cosine-score provenance.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SimilarSymbolHit {
    symbol: VectorSearchHit,
    score: f64,
}

impl SimilarSymbolHit {
    pub(crate) const fn new(symbol: VectorSearchHit, score: f64) -> Self {
        Self { symbol, score }
    }

    /// Current-generation symbol metadata for this neighbor.
    #[must_use]
    pub const fn symbol(&self) -> &VectorSearchHit {
        &self.symbol
    }

    /// Cosine similarity, where larger is more similar.
    #[must_use]
    pub const fn score(&self) -> f64 {
        self.score
    }
}

/// Model-scoped semantic neighbors for one exact source symbol.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SimilarSymbolsResult {
    model: EmbeddingModelSelector,
    source_symbol_id: SymbolId,
    hits: Vec<SimilarSymbolHit>,
    truncated: bool,
}

pub(super) struct SimilarSymbolsResultInput {
    pub(super) model: EmbeddingModelSelector,
    pub(super) source_symbol_id: SymbolId,
    pub(super) hits: Vec<SimilarSymbolHit>,
    pub(super) truncated: bool,
}

impl SimilarSymbolsResult {
    pub(super) fn new(input: SimilarSymbolsResultInput) -> Self {
        let SimilarSymbolsResultInput {
            model,
            source_symbol_id,
            hits,
            truncated,
        } = input;
        Self {
            model,
            source_symbol_id,
            hits,
            truncated,
        }
    }

    /// Exact active model used to compare stored vectors.
    #[must_use]
    pub const fn model(&self) -> &EmbeddingModelSelector {
        &self.model
    }

    /// Exact source symbol used as the query vector.
    #[must_use]
    pub const fn source_symbol_id(&self) -> &SymbolId {
        &self.source_symbol_id
    }

    /// Score-descending, deterministic symbol neighbors.
    #[must_use]
    pub fn hits(&self) -> &[SimilarSymbolHit] {
        &self.hits
    }

    /// Whether the requested result bound omitted additional matches.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl VectorSearchHit {
    /// Exact generation searched under the project publication lock.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Stable logical document identity.
    #[must_use]
    pub const fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    /// Owning file when this hit is backed by a file or symbol fact.
    #[must_use]
    pub const fn file_id(&self) -> Option<&FileId> {
        self.file_id.as_ref()
    }

    /// Owning symbol for symbol-level evidence.
    #[must_use]
    pub const fn symbol_id(&self) -> Option<&SymbolId> {
        self.symbol_id.as_ref()
    }

    /// Project-normalized source path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Stored language identifier.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Intent-routing search-document kind.
    #[must_use]
    pub const fn document_kind(&self) -> DocumentKind {
        self.document_kind
    }

    /// Exact qualified-name text shared with lexical retrieval metadata.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Exact extracted symbol category when retained in current search metadata.
    #[must_use]
    pub const fn symbol_kind(&self) -> Option<SymbolKind> {
        self.symbol_kind
    }

    /// Bounded current-generation text reserved for an optional reranker call.
    ///
    /// This source-bearing field is deliberately omitted from serialization.
    #[must_use]
    pub fn rerank_text(&self) -> Option<&str> {
        self.rerank_text.as_deref()
    }

    /// Exact native cosine distance; smaller is nearer.
    #[must_use]
    pub const fn distance(&self) -> f64 {
        self.distance
    }
}

/// Audited model retirement tied to one fully ready replacement.
pub struct RetireEmbeddingModelRequest {
    pub(crate) retiring: EmbeddingModelSelector,
    pub(crate) replacement: EmbeddingModelSelector,
    pub(crate) statement_timeout: Duration,
}

impl RetireEmbeddingModelRequest {
    /// Require distinct exact model identities and a bounded audit transaction.
    /// # Errors
    ///
    /// Returns an error if retiring and replacement model IDs are identical or
    /// the audit deadline is invalid.
    pub fn new(
        retiring: EmbeddingModelSelector,
        replacement: EmbeddingModelSelector,
        statement_timeout: Duration,
    ) -> Result<Self, SemanticStorageError> {
        if retiring.model_id == replacement.model_id {
            return Err(invalid("replacement_model"));
        }
        validate_timeout(statement_timeout)?;
        Ok(Self {
            retiring,
            replacement,
            statement_timeout,
        })
    }
}

/// Secret-, source-, and deployment-safe semantic storage failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SemanticStorageError {
    /// One caller-controlled field violated its fixed bound/domain.
    #[error("invalid {field} in Cartograph semantic storage request")]
    InvalidInput {
        /// Caller-controlled field that violated its documented bound.
        field: &'static str,
    },
    /// The model UUID or fingerprint is already registered with different metadata.
    #[error("embedding model identity conflicts with the existing registry")]
    ModelConflict,
    /// The requested model does not exist.
    #[error("embedding model is not registered")]
    ModelNotFound,
    /// The registered fingerprint or dimension differs from the exact selector.
    #[error("embedding model selector does not match the registry")]
    ModelMismatch,
    /// New writes and searches are forbidden after retirement.
    #[error("embedding model is retired")]
    ModelRetired,
    /// A replacement lacked complete current-generation coverage or a ready vector reader.
    #[error("replacement embedding model is not ready for audited retirement")]
    ReplacementNotReady,
    /// The project or current generation is absent.
    #[error("current project generation is unavailable")]
    CurrentGenerationUnavailable,
    /// A generation-bound page/write was invalidated by publication.
    #[error("current project generation changed during semantic storage work")]
    CurrentGenerationChanged,
    /// A requested document was absent from the exact current generation.
    #[error("embedding document is not present in the current generation")]
    DocumentNotFound,
    /// The source symbol has no stored vector for the selected/current active model.
    #[error("source symbol has no ready active embedding")]
    SourceEmbeddingUnavailable,
    /// More than one active model can answer and the caller did not choose one.
    #[error("multiple active embedding models are available; select an exact model ID")]
    AmbiguousActiveModels,
    /// The exact source text changed or the caller paired the wrong digest.
    #[error("embedding source digest does not match the current document")]
    SourceDigestChanged,
    /// One document cannot fit within the caller's explicit retained byte budget.
    #[error("embedding source document exceeds the requested page byte bound")]
    DocumentTooLarge,
    /// Top-K was attempted without all required readiness evidence.
    #[error("semantic retrieval is not ready: {state:?}")]
    NotReady {
        /// Exact readiness state that blocked semantic retrieval.
        state: SemanticReadinessState,
    },
    /// PostgreSQL could not allocate the shared-memory segment for HNSW creation.
    #[error("Cartograph PostgreSQL HNSW creation failed because shared memory is unavailable")]
    HnswCreateSharedMemoryUnavailable,
    /// A stored identity, enum, count, or score violated the schema contract.
    #[error("Cartograph semantic storage violates the {field} domain contract")]
    CorruptStoredValue {
        /// Stored field whose value violated the semantic storage contract.
        field: &'static str,
    },
    /// Driver details are redacted because they may contain deployment/source data.
    #[error("Cartograph native semantic operation failed during {operation}")]
    DatabaseOperation {
        /// Bounded operation label identifying the failed native phase.
        operation: &'static str,
    },
}

pub(crate) fn validate_timeout(timeout: Duration) -> Result<(), SemanticStorageError> {
    if timeout.is_zero() || timeout > MAXIMUM_STATEMENT_TIMEOUT {
        Err(invalid("statement_timeout"))
    } else {
        Ok(())
    }
}

fn validate_vector(vector: &[f32]) -> Result<(), SemanticStorageError> {
    if vector.is_empty()
        || vector.len() > MAXIMUM_MODEL_DIMENSION
        || vector.iter().any(|value| !value.is_finite())
    {
        return Err(invalid("embedding"));
    }
    let magnitude = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>();
    if !magnitude.is_finite() || magnitude == 0.0 {
        Err(invalid("embedding"))
    } else {
        Ok(())
    }
}

fn bounded_text(
    mut value: String,
    field: &'static str,
    maximum: usize,
) -> Result<String, SemanticStorageError> {
    value.truncate(value.trim_end().len());
    let leading_whitespace = value.len().saturating_sub(value.trim_start().len());
    if leading_whitespace > 0 {
        value.drain(..leading_whitespace);
    }
    if value.is_empty()
        || value.len() > maximum
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        Err(invalid(field))
    } else {
        Ok(value)
    }
}

pub(crate) const fn invalid(field: &'static str) -> SemanticStorageError {
    SemanticStorageError::InvalidInput { field }
}

pub(crate) const fn database_error(operation: &'static str) -> SemanticStorageError {
    SemanticStorageError::DatabaseOperation { operation }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "11111111-1111-8111-8111-111111111111";
    const DOCUMENT: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const PROJECT: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const GENERATION: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const SYMBOL: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn model_vector_page_and_batch_boundaries_fail_before_database_work() {
        let model_id =
            ModelId::parse(MODEL).unwrap_or_else(|error| panic!("model fixture failed: {error}"));
        let digest = ContentDigest::parse(DIGEST)
            .unwrap_or_else(|error| panic!("digest fixture failed: {error}"));
        assert!(
            EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
                model_id: model_id.clone(),
                fingerprint: digest.clone(),
                provider: "provider".to_owned(),
                model_name: "model".to_owned(),
                dimension: 0,
                normalization: EmbeddingNormalization::None,
            })
            .is_err()
        );
        assert!(
            EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
                model_id,
                fingerprint: digest.clone(),
                provider: "provider".to_owned(),
                model_name: "model".to_owned(),
                dimension: u16::try_from(MAXIMUM_MODEL_DIMENSION + 1).unwrap_or(u16::MAX),
                normalization: EmbeddingNormalization::None,
            })
            .is_err()
        );
        let document = DocumentId::parse(DOCUMENT)
            .unwrap_or_else(|error| panic!("document fixture failed: {error}"));
        assert!(EmbeddingUpsertRow::new(document.clone(), digest.clone(), Vec::new()).is_err());
        assert!(EmbeddingUpsertRow::new(document.clone(), digest.clone(), vec![f32::NAN]).is_err());
        assert!(EmbeddingUpsertRow::new(document, digest, vec![0.0, 0.0]).is_err());
    }

    #[test]
    fn similar_symbol_requests_reject_invalid_scores_limits_and_deadlines() {
        let input = |limit, minimum_score, statement_timeout| SimilarSymbolsInput {
            project_id: ProjectId::parse(PROJECT)
                .unwrap_or_else(|error| panic!("project fixture failed: {error}")),
            expected_generation_id: GenerationId::parse(GENERATION)
                .unwrap_or_else(|error| panic!("generation fixture failed: {error}")),
            source_symbol_id: SymbolId::parse(SYMBOL)
                .unwrap_or_else(|error| panic!("symbol fixture failed: {error}")),
            model_id: None,
            limit,
            minimum_score,
            same_language: false,
            statement_timeout,
        };
        assert!(SimilarSymbolsRequest::new(input(1, 0.0, Duration::from_secs(1))).is_ok());
        assert!(SimilarSymbolsRequest::new(input(50, 1.0, Duration::from_secs(1))).is_ok());
        for invalid in [
            input(0, 0.3, Duration::from_secs(1)),
            input(51, 0.3, Duration::from_secs(1)),
            input(5, -0.1, Duration::from_secs(1)),
            input(5, 1.1, Duration::from_secs(1)),
            input(5, f64::NAN, Duration::from_secs(1)),
            input(5, 0.3, Duration::ZERO),
        ] {
            assert!(SimilarSymbolsRequest::new(invalid).is_err());
        }
    }

    #[test]
    fn vector_search_hit_keeps_rerank_text_internal_and_debug_safe() {
        let source_text = "fn private_candidate() { secret_call(); }";
        let hit = VectorSearchHit {
            generation_id: GenerationId::parse(GENERATION)
                .unwrap_or_else(|error| panic!("generation fixture failed: {error}")),
            document_id: DocumentId::parse(DOCUMENT)
                .unwrap_or_else(|error| panic!("document fixture failed: {error}")),
            file_id: None,
            symbol_id: Some(
                SymbolId::parse(SYMBOL)
                    .unwrap_or_else(|error| panic!("symbol fixture failed: {error}")),
            ),
            path: "src/private.rs".to_owned(),
            language: "rust".to_owned(),
            document_kind: DocumentKind::Symbol,
            qualified_name: "private_candidate".to_owned(),
            symbol_kind: Some(SymbolKind::Function),
            distance: 0.25,
            rerank_text: Some(source_text.to_owned()),
        };

        assert_eq!(hit.rerank_text(), Some(source_text));
        let debug = format!("{hit:?}");
        assert!(!debug.contains(source_text));
        assert!(debug.contains("rerank_text_bytes"));
        let serialized = serde_json::to_string(&hit)
            .unwrap_or_else(|error| panic!("hit serialization failed: {error}"));
        assert!(!serialized.contains(source_text));
        assert!(!serialized.contains("rerank_text"));
    }
}
