use std::time::Duration;

use cartograph_db::{
    EmbeddingModelRegistration, EmbeddingModelRegistrationInput, EmbeddingModelSelector,
    EmbeddingNormalization, SemanticReadinessRequest, SemanticReadinessState, SemanticStorageError,
    StorageError, VectorSearchHit, VectorSearchInput, VectorSearchRequest,
};
use cartograph_domain::{GenerationId, NormalizedPath, ProjectId, SourceLanguage};
use cartograph_llm::{
    EmbeddingSettings, OpenAiEmbeddingClient, OpenAiRerankClient, RerankSettings,
};
use cartograph_search::{
    ChannelCandidate, ChannelResults, DeterministicRetriever, GenerationLexicalRequest,
    HybridSearchInput, HybridSearchPacket, LexicalQuery, RerankReport, RetrievalChannel,
    RetrievalChannels, RetrievalDocument, RetrievalDocumentInput, RetrievalError,
    RetrievalPreference, SearchMode, SemanticReadiness, TaskIntent, fuse_search,
};

use crate::{ProjectCancellation, ProjectError, ProjectRuntime};

const DEFAULT_CANDIDATE_LIMIT: u16 = 20;
const MAXIMUM_CANDIDATE_LIMIT: u16 = 100;
const SEMANTIC_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(30);
const SEMANTIC_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const RERANK_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(30);
const MODEL_PROVIDER: &str = "openai-compatible";
const GENERATION_ATTEMPTS: usize = 2;

enum SemanticClientState {
    NotConfigured,
    Unavailable,
    Ready(Box<OpenAiEmbeddingClient>),
}

enum RerankClientState {
    NotRequested,
    NotConfigured,
    Unavailable { model: Option<String> },
    Ready(Box<OpenAiRerankClient>),
}

#[derive(Clone, Copy)]
enum RetrievalStageError {
    RequestCancelled,
    GenerationChanged,
    Failed,
}

/// Validated deterministic/auto/hybrid policy for one natural-language retrieval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetrievalOptions {
    mode: SearchMode,
    candidate_limit: u16,
    result_limit: u16,
    rerank: bool,
}

/// One bounded natural-language retrieval request shared by CLI, MCP, and tests.
pub struct RetrievalRequest {
    project_id: ProjectId,
    query: String,
    options: RetrievalOptions,
}

impl RetrievalRequest {
    /// Bind project identity, redacted query text, and execution policy.
    #[must_use]
    pub fn new(project_id: ProjectId, query: impl Into<String>, options: RetrievalOptions) -> Self {
        Self {
            project_id,
            query: query.into(),
            options,
        }
    }
}

impl std::fmt::Debug for RetrievalRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetrievalRequest")
            .field("project_id", &self.project_id)
            .field("query", &"<redacted>")
            .field("options", &self.options)
            .finish()
    }
}

/// Explicit embedding-client execution used by live tests and embedded hosts.
pub struct RetrievalClientRequest {
    request: RetrievalRequest,
    client: OpenAiEmbeddingClient,
    cancellation: ProjectCancellation,
}

impl RetrievalClientRequest {
    /// Attach a validated endpoint client and caller-owned cancellation.
    #[must_use]
    pub const fn new(
        request: RetrievalRequest,
        client: OpenAiEmbeddingClient,
        cancellation: ProjectCancellation,
    ) -> Self {
        Self {
            request,
            client,
            cancellation,
        }
    }
}

impl RetrievalOptions {
    /// Select a mode and a bounded Top-K candidate count.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `candidate_limit` is zero
    /// or exceeds the per-channel recall-window ceiling.
    pub const fn new(mode: SearchMode, candidate_limit: u16) -> Result<Self, ProjectError> {
        if candidate_limit == 0 || candidate_limit > MAXIMUM_CANDIDATE_LIMIT {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            mode,
            candidate_limit,
            result_limit: candidate_limit,
            rerank: true,
        })
    }

    /// Skip the configured cross-encoder for a caller that ranks candidates
    /// itself, such as decision-guided exploration. The semantic channel keeps
    /// its vector order and reports reranking as not requested.
    #[must_use]
    pub const fn without_rerank(mut self) -> Self {
        self.rerank = false;
        self
    }

    /// Requested deterministic, hybrid, or automatic execution policy.
    #[must_use]
    pub const fn mode(self) -> SearchMode {
        self.mode
    }

    /// Maximum independently ranked candidates admitted from each channel.
    #[must_use]
    pub const fn candidate_limit(self) -> u16 {
        self.candidate_limit
    }

    /// Maximum fused results returned after the channel candidate windows are joined.
    #[must_use]
    pub const fn result_limit(self) -> u16 {
        self.result_limit
    }

    /// Bound returned results independently from each channel's recall window.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `result_limit` is zero or
    /// exceeds the fused-result ceiling.
    pub const fn with_result_limit(mut self, result_limit: u16) -> Result<Self, ProjectError> {
        if result_limit == 0 || result_limit > MAXIMUM_CANDIDATE_LIMIT {
            return Err(ProjectError::InvalidOptions);
        }
        self.result_limit = result_limit;
        Ok(self)
    }

    /// Replace only the channel policy while retaining the validated bound.
    #[must_use]
    pub const fn with_mode(mut self, mode: SearchMode) -> Self {
        self.mode = mode;
        self
    }
}

impl Default for RetrievalOptions {
    fn default() -> Self {
        Self {
            mode: SearchMode::Auto,
            candidate_limit: DEFAULT_CANDIDATE_LIMIT,
            result_limit: DEFAULT_CANDIDATE_LIMIT,
            rerank: true,
        }
    }
}

/// Concurrent channel result plus the exact semantic readiness used by policy selection.
pub struct PreparedRetrieval {
    semantic_readiness: SemanticReadiness,
    preference: RetrievalPreference,
    channels: RetrievalChannels,
}

impl PreparedRetrieval {
    /// Semantic readiness observed for this exact query attempt.
    #[must_use]
    pub const fn semantic_readiness(&self) -> SemanticReadiness {
        self.semantic_readiness
    }

    /// Consume the preparation into the channel join point used by context assembly.
    #[must_use]
    pub fn into_channels(self) -> RetrievalChannels {
        self.channels
    }

    fn into_packet(self, options: RetrievalOptions) -> Result<HybridSearchPacket, ProjectError> {
        fuse_search(
            HybridSearchInput::new(options.mode, self.semantic_readiness, options.result_limit)
                .map_err(|_| ProjectError::RetrievalOperationFailed)?
                .with_preference(self.preference)
                .with_channels(self.channels),
        )
        .map_err(|_| ProjectError::RetrievalOperationFailed)
    }
}

impl ProjectRuntime {
    /// Execute bounded lexical and eligible semantic retrieval concurrently.
    /// # Errors
    ///
    /// Returns an error when the query cannot form a bounded lexical request,
    /// current-generation channel evidence cannot be read consistently, or
    /// retrieval invariants fail. Endpoint unavailability degrades explicitly.
    pub async fn prepare_retrieval(
        &self,
        request: RetrievalRequest,
    ) -> Result<PreparedRetrieval, ProjectError> {
        self.prepare_retrieval_with_cancellation(request, ProjectCancellation::new())
            .await
    }

    /// Prepare independently ranked channels with caller-owned cancellation.
    /// # Errors
    ///
    /// Returns an error when the query cannot form a bounded lexical request,
    /// current-generation channel evidence cannot be read consistently,
    /// retrieval invariants fail, or `cancellation` wins. Endpoint
    /// unavailability degrades explicitly.
    pub async fn prepare_retrieval_with_cancellation(
        &self,
        request: RetrievalRequest,
        cancellation: ProjectCancellation,
    ) -> Result<PreparedRetrieval, ProjectError> {
        let semantic_client = semantic_client_from_project(
            request.options.mode,
            self.project_root_for_host_operations(),
        );
        let rerank_client = if request.options.rerank {
            rerank_client_from_project(
                request.options.mode,
                self.project_root_for_host_operations(),
            )
        } else {
            RerankClientState::NotRequested
        };
        self.prepare_retrieval_inner(RetrievalExecutionRequest {
            request,
            semantic_client,
            rerank_client,
            cancellation,
        })
        .await
    }

    /// Prepare channels with an explicitly validated client for tests and embedded hosts.
    /// # Errors
    ///
    /// Returns an error when the query cannot form a bounded lexical request,
    /// current-generation channel evidence cannot be read consistently,
    /// retrieval invariants fail, or the supplied cancellation wins. Endpoint
    /// failures degrade to readiness evidence when a channel can still answer.
    pub async fn prepare_retrieval_with_client(
        &self,
        input: RetrievalClientRequest,
    ) -> Result<PreparedRetrieval, ProjectError> {
        let rerank_client = rerank_client_without_project(input.request.options.mode);
        self.prepare_retrieval_inner(RetrievalExecutionRequest {
            request: input.request,
            semantic_client: SemanticClientState::Ready(Box::new(input.client)),
            rerank_client,
            cancellation: input.cancellation,
        })
        .await
    }

    async fn prepare_retrieval_inner(
        &self,
        input: RetrievalExecutionRequest,
    ) -> Result<PreparedRetrieval, ProjectError> {
        let RetrievalExecutionRequest {
            request:
                RetrievalRequest {
                    project_id,
                    query,
                    options,
                },
            semantic_client,
            rerank_client,
            cancellation,
        } = input;
        for attempt in 0..GENERATION_ATTEMPTS {
            match self
                .prepare_retrieval_attempt(RetrievalAttemptRequest {
                    project_id: &project_id,
                    query: &query,
                    options,
                    semantic_client: &semantic_client,
                    rerank_client: &rerank_client,
                    cancellation: &cancellation,
                })
                .await
            {
                Err(RetrievalStageError::GenerationChanged) if attempt == 0 => {}
                Err(error) => return Err(project_retrieval_error(error)),
                Ok(prepared) => return Ok(prepared),
            }
        }
        Err(ProjectError::RetrievalOperationFailed)
    }

    async fn prepare_retrieval_attempt(
        &self,
        input: RetrievalAttemptRequest<'_>,
    ) -> Result<PreparedRetrieval, RetrievalStageError> {
        let RetrievalAttemptRequest {
            project_id,
            query,
            options,
            semantic_client,
            rerank_client,
            cancellation,
        } = input;
        let preference = TaskIntent::classify(query).retrieval_preference(query);
        let generation = self
            .database()
            .current_generation_record(project_id)
            .await
            .map_err(|error| storage_stage_error(&error))?;
        let Some(generation) = generation else {
            return prepared_without_generation(options.mode, preference, rerank_client);
        };
        let expected_generation_id = generation.generation_id().clone();
        let lexical_query = LexicalQuery::for_code_search(query, options.candidate_limit)
            .map_err(|_| RetrievalStageError::Failed)?;
        let retriever = DeterministicRetriever::new(self.database().clone());
        if options.mode == SearchMode::Deterministic {
            return deterministic_retrieval(DeterministicRetrievalRequest {
                runtime: self,
                retriever,
                project_id,
                expected_generation_id,
                lexical_query,
                preference,
                cancellation,
            })
            .await;
        }
        hybrid_retrieval(HybridRetrievalRequest {
            runtime: self,
            retriever,
            project_id,
            expected_generation_id,
            lexical_query,
            query,
            options,
            semantic_client,
            rerank_client,
            preference,
            cancellation,
        })
        .await
    }

    /// Return one compact fused packet suitable for CLI and MCP `find` consumers.
    /// # Errors
    ///
    /// Returns an error when channel preparation fails or the prepared channel
    /// ranks, generation, or configured result bound cannot form a valid fused packet.
    pub async fn search(
        &self,
        request: RetrievalRequest,
    ) -> Result<HybridSearchPacket, ProjectError> {
        self.search_with_cancellation(request, ProjectCancellation::new())
            .await
    }

    /// Search with explicit cancellation across the endpoint and both PostgreSQL channels.
    /// # Errors
    ///
    /// Returns an error when channel preparation fails, `cancellation` wins, or
    /// the prepared ranks, generation, or result bound cannot form a valid fused packet.
    pub async fn search_with_cancellation(
        &self,
        request: RetrievalRequest,
        cancellation: ProjectCancellation,
    ) -> Result<HybridSearchPacket, ProjectError> {
        let options = request.options;
        self.prepare_retrieval_with_cancellation(request, cancellation)
            .await?
            .into_packet(options)
    }

    /// Search through an explicit client without mutating process environment configuration.
    /// # Errors
    ///
    /// Returns an error when explicit-client channel preparation fails,
    /// cancellation wins, or the prepared ranks, generation, or result bound
    /// cannot form a valid fused packet.
    pub async fn search_with_client(
        &self,
        input: RetrievalClientRequest,
    ) -> Result<HybridSearchPacket, ProjectError> {
        let options = input.request.options;
        self.prepare_retrieval_with_client(input)
            .await?
            .into_packet(options)
    }
}

struct RetrievalExecutionRequest {
    request: RetrievalRequest,
    semantic_client: SemanticClientState,
    rerank_client: RerankClientState,
    cancellation: ProjectCancellation,
}

struct RetrievalAttemptRequest<'a> {
    project_id: &'a ProjectId,
    query: &'a str,
    options: RetrievalOptions,
    semantic_client: &'a SemanticClientState,
    rerank_client: &'a RerankClientState,
    cancellation: &'a ProjectCancellation,
}

struct DeterministicRetrievalRequest<'request> {
    runtime: &'request ProjectRuntime,
    retriever: DeterministicRetriever,
    project_id: &'request ProjectId,
    expected_generation_id: GenerationId,
    lexical_query: LexicalQuery,
    preference: RetrievalPreference,
    cancellation: &'request ProjectCancellation,
}

struct HybridRetrievalRequest<'request> {
    runtime: &'request ProjectRuntime,
    retriever: DeterministicRetriever,
    project_id: &'request ProjectId,
    expected_generation_id: GenerationId,
    lexical_query: LexicalQuery,
    query: &'request str,
    options: RetrievalOptions,
    semantic_client: &'request SemanticClientState,
    rerank_client: &'request RerankClientState,
    preference: RetrievalPreference,
    cancellation: &'request ProjectCancellation,
}

struct LexicalChannelRequest {
    retriever: DeterministicRetriever,
    project_id: ProjectId,
    expected_generation_id: GenerationId,
    query: LexicalQuery,
    cancellation: ProjectCancellation,
}

struct SemanticChannelRequest<'runtime> {
    runtime: &'runtime ProjectRuntime,
    project_id: ProjectId,
    expected_generation_id: GenerationId,
    query: String,
    limit: u16,
    client: &'runtime SemanticClientState,
    reranker: &'runtime RerankClientState,
    cancellation: ProjectCancellation,
}

struct SemanticChannelResult {
    readiness: SemanticReadiness,
    channel: Option<ChannelResults>,
    rerank: RerankReport,
}

struct SemanticEmbedding {
    selector: EmbeddingModelSelector,
    vector: Vec<f32>,
}

fn prepared_without_generation(
    mode: SearchMode,
    preference: RetrievalPreference,
    reranker: &RerankClientState,
) -> Result<PreparedRetrieval, RetrievalStageError> {
    let lexical = ChannelResults::new(RetrievalChannel::Lexical, Vec::new())
        .map_err(|_| RetrievalStageError::Failed)?;
    let channels = RetrievalChannels::new()
        .with_channel(lexical)
        .map_err(|_| RetrievalStageError::Failed)?
        .with_rerank_report(rerank_report_without_candidates(reranker)?);
    let semantic_readiness = if mode == SearchMode::Deterministic {
        SemanticReadiness::NotConfigured
    } else {
        SemanticReadiness::NotIndexed
    };
    Ok(PreparedRetrieval {
        semantic_readiness,
        preference,
        channels,
    })
}

async fn deterministic_retrieval(
    input: DeterministicRetrievalRequest<'_>,
) -> Result<PreparedRetrieval, RetrievalStageError> {
    let DeterministicRetrievalRequest {
        runtime,
        retriever,
        project_id,
        expected_generation_id,
        lexical_query,
        preference,
        cancellation,
    } = input;
    let lexical = lexical_channel(LexicalChannelRequest {
        retriever,
        project_id: project_id.clone(),
        expected_generation_id: expected_generation_id.clone(),
        query: lexical_query,
        cancellation: cancellation.clone(),
    })
    .await?;
    ensure_generation_current(runtime, project_id, &expected_generation_id).await?;
    let channels = optional_channel(Some(lexical))
        .map_err(|_| RetrievalStageError::Failed)?
        .with_rerank_report(RerankReport::not_requested());
    Ok(PreparedRetrieval {
        semantic_readiness: SemanticReadiness::NotConfigured,
        preference,
        channels,
    })
}

async fn hybrid_retrieval(
    input: HybridRetrievalRequest<'_>,
) -> Result<PreparedRetrieval, RetrievalStageError> {
    let HybridRetrievalRequest {
        runtime,
        retriever,
        project_id,
        expected_generation_id,
        lexical_query,
        query,
        options,
        semantic_client,
        rerank_client,
        preference,
        cancellation,
    } = input;
    let lexical = lexical_channel(LexicalChannelRequest {
        retriever,
        project_id: project_id.clone(),
        expected_generation_id: expected_generation_id.clone(),
        query: lexical_query,
        cancellation: cancellation.clone(),
    });
    let semantic = semantic_channel(SemanticChannelRequest {
        runtime,
        project_id: project_id.clone(),
        expected_generation_id: expected_generation_id.clone(),
        query: query.to_owned(),
        limit: options.candidate_limit,
        client: semantic_client,
        reranker: rerank_client,
        cancellation: cancellation.clone(),
    });
    let (lexical, semantic) = tokio::join!(lexical, semantic);
    let lexical = optional_lexical_result(lexical)?;
    let semantic = semantic?;
    if lexical.is_none() && semantic.channel.is_none() {
        return Err(RetrievalStageError::Failed);
    }
    let mut channels = optional_channel(lexical).map_err(|_| RetrievalStageError::Failed)?;
    if let Some(channel) = semantic.channel {
        channels = channels
            .with_channel(channel)
            .map_err(|_| RetrievalStageError::Failed)?;
    }
    channels = channels.with_rerank_report(semantic.rerank);
    ensure_generation_current(runtime, project_id, &expected_generation_id).await?;
    Ok(PreparedRetrieval {
        semantic_readiness: semantic.readiness,
        preference,
        channels,
    })
}

fn optional_lexical_result(
    result: Result<ChannelResults, RetrievalStageError>,
) -> Result<Option<ChannelResults>, RetrievalStageError> {
    match result {
        Ok(channel) => Ok(Some(channel)),
        Err(RetrievalStageError::RequestCancelled) => Err(RetrievalStageError::RequestCancelled),
        Err(RetrievalStageError::GenerationChanged) => Err(RetrievalStageError::GenerationChanged),
        Err(RetrievalStageError::Failed) => Ok(None),
    }
}

fn semantic_client_from_project(
    mode: SearchMode,
    project_root: &std::path::Path,
) -> SemanticClientState {
    if mode == SearchMode::Deterministic {
        return SemanticClientState::NotConfigured;
    }
    match EmbeddingSettings::try_from_project(project_root) {
        Ok(Some(settings)) => OpenAiEmbeddingClient::new(settings)
            .map_or(SemanticClientState::Unavailable, |client| {
                SemanticClientState::Ready(Box::new(client))
            }),
        Ok(None) => SemanticClientState::NotConfigured,
        Err(_) => SemanticClientState::Unavailable,
    }
}

fn rerank_client_without_project(mode: SearchMode) -> RerankClientState {
    if mode == SearchMode::Deterministic {
        RerankClientState::NotRequested
    } else {
        RerankClientState::NotConfigured
    }
}

fn rerank_client_from_project(
    mode: SearchMode,
    project_root: &std::path::Path,
) -> RerankClientState {
    if mode == SearchMode::Deterministic {
        return RerankClientState::NotRequested;
    }
    match RerankSettings::try_from_project(project_root) {
        Ok(Some(settings)) => {
            let model = settings.model().to_owned();
            OpenAiRerankClient::new(settings).map_or(
                RerankClientState::Unavailable { model: Some(model) },
                |client| RerankClientState::Ready(Box::new(client)),
            )
        }
        Ok(None) => RerankClientState::NotConfigured,
        Err(_) => RerankClientState::Unavailable { model: None },
    }
}

async fn lexical_channel(
    input: LexicalChannelRequest,
) -> Result<ChannelResults, RetrievalStageError> {
    let LexicalChannelRequest {
        retriever,
        project_id,
        expected_generation_id,
        query,
        cancellation,
    } = input;
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(RetrievalStageError::RequestCancelled),
        result = retriever.lexical_channel_for_generation(GenerationLexicalRequest::new(
            project_id,
            expected_generation_id,
            query,
        )) => {
            result.map_err(|error| retrieval_stage_error(&error))
        },
    }
}

async fn semantic_embedding(
    client: &OpenAiEmbeddingClient,
    query: &str,
    cancellation: &ProjectCancellation,
) -> Result<Option<SemanticEmbedding>, RetrievalStageError> {
    let inputs = [query.to_owned()];
    let batch = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RetrievalStageError::RequestCancelled),
        result = tokio::time::timeout(SEMANTIC_ENDPOINT_TIMEOUT, client.embed(&inputs)) => match result {
            Ok(Ok(batch)) => batch,
            Ok(Err(_)) | Err(_) => return Ok(None),
        },
    };
    let Ok(dimension) = u16::try_from(batch.dimension()) else {
        return Ok(None);
    };
    let Ok(registration) = model_registration(client, dimension) else {
        return Ok(None);
    };
    let Some(vector) = batch.vectors().first() else {
        return Ok(None);
    };
    Ok(Some(SemanticEmbedding {
        selector: registration.selector(),
        vector: vector.values().to_vec(),
    }))
}

async fn semantic_channel(
    input: SemanticChannelRequest<'_>,
) -> Result<SemanticChannelResult, RetrievalStageError> {
    let SemanticChannelRequest {
        runtime,
        project_id,
        expected_generation_id,
        query,
        limit,
        client,
        reranker,
        cancellation,
    } = input;
    let client = match client {
        SemanticClientState::NotConfigured => {
            return semantic_without_candidates(SemanticReadiness::NotConfigured, reranker);
        }
        SemanticClientState::Unavailable => {
            return semantic_without_candidates(SemanticReadiness::Unavailable, reranker);
        }
        SemanticClientState::Ready(client) => client,
    };
    let Some(embedding) = semantic_embedding(client, &query, &cancellation).await? else {
        return semantic_without_candidates(SemanticReadiness::Unavailable, reranker);
    };
    let readiness_request = SemanticReadinessRequest::new(
        project_id.clone(),
        embedding.selector.clone(),
        SEMANTIC_QUERY_TIMEOUT,
    )
    .map_err(|_| RetrievalStageError::Failed)?;
    let readiness_report = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RetrievalStageError::RequestCancelled),
        result = runtime.database().semantic_readiness(readiness_request) => match result {
            Ok(report) => report,
            Err(SemanticStorageError::CurrentGenerationChanged) => {
                return Err(RetrievalStageError::GenerationChanged);
            }
            Err(_) => return semantic_without_candidates(SemanticReadiness::Unavailable, reranker),
        },
    };
    if readiness_report
        .generation_id()
        .is_some_and(|generation| generation != &expected_generation_id)
        || (readiness_report.generation_id().is_none()
            && readiness_report.state() == SemanticReadinessState::NoCurrentGeneration)
    {
        return Err(RetrievalStageError::GenerationChanged);
    }
    let readiness = semantic_readiness_from_database(readiness_report.state());
    if readiness != SemanticReadiness::Ready {
        return semantic_without_candidates(readiness, reranker);
    }
    let request = VectorSearchRequest::new(VectorSearchInput {
        project_id,
        expected_generation_id,
        model: embedding.selector,
        vector: embedding.vector,
        limit,
        statement_timeout: SEMANTIC_QUERY_TIMEOUT,
    })
    .map_err(|_| RetrievalStageError::Failed)?;
    let hits = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RetrievalStageError::RequestCancelled),
        result = runtime.database().vector_top_k(request) => match result {
            Ok(hits) => hits,
            Err(SemanticStorageError::NotReady { state }) => {
                return semantic_without_candidates(
                    semantic_readiness_from_database(state),
                    reranker,
                );
            }
            Err(SemanticStorageError::CurrentGenerationChanged) => {
                return Err(RetrievalStageError::GenerationChanged);
            }
            Err(_) => return semantic_without_candidates(SemanticReadiness::Unavailable, reranker),
        },
    };
    let channel = semantic_results(&hits).map_err(|_| RetrievalStageError::Failed)?;
    let (channel, rerank) = rerank_semantic_channel(RerankSemanticRequest {
        query: &query,
        hits: &hits,
        channel,
        client: reranker,
        cancellation: &cancellation,
    })
    .await?;
    Ok(SemanticChannelResult {
        readiness: SemanticReadiness::Ready,
        channel: Some(channel),
        rerank,
    })
}

struct RerankSemanticRequest<'request> {
    query: &'request str,
    hits: &'request [VectorSearchHit],
    channel: ChannelResults,
    client: &'request RerankClientState,
    cancellation: &'request ProjectCancellation,
}

async fn rerank_semantic_channel(
    input: RerankSemanticRequest<'_>,
) -> Result<(ChannelResults, RerankReport), RetrievalStageError> {
    let RerankSemanticRequest {
        query,
        hits,
        channel,
        client,
        cancellation,
    } = input;
    let client = match client {
        RerankClientState::NotRequested => {
            return Ok((channel, RerankReport::not_requested()));
        }
        RerankClientState::NotConfigured => {
            return Ok((channel, RerankReport::not_configured()));
        }
        RerankClientState::Unavailable { model } => {
            return Ok((
                channel,
                RerankReport::unavailable(model.clone())
                    .map_err(|_| RetrievalStageError::Failed)?,
            ));
        }
        RerankClientState::Ready(client) => client,
    };
    if hits.is_empty() {
        return Ok((
            channel,
            RerankReport::skipped_no_candidates(client.model())
                .map_err(|_| RetrievalStageError::Failed)?,
        ));
    }
    let documents = hits
        .iter()
        .map(|hit| {
            hit.rerank_text()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
        })
        .collect::<Option<Vec<_>>>();
    let Some(documents) = documents else {
        return Ok((
            channel,
            RerankReport::skipped_no_text(client.model())
                .map_err(|_| RetrievalStageError::Failed)?,
        ));
    };
    let batch = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RetrievalStageError::RequestCancelled),
        result = tokio::time::timeout(
            RERANK_ENDPOINT_TIMEOUT,
            client.rerank(query, &documents),
        ) => match result {
            Ok(Ok(batch)) => batch,
            Ok(Err(_)) | Err(_) => {
                return Ok((
                    channel,
                    RerankReport::unavailable(Some(client.model().to_owned()))
                        .map_err(|_| RetrievalStageError::Failed)?,
                ));
            }
        },
    };
    let Ok(reranked) = apply_rerank_scores(&channel, batch.scores()) else {
        return Ok((
            channel,
            RerankReport::unavailable(Some(client.model().to_owned()))
                .map_err(|_| RetrievalStageError::Failed)?,
        ));
    };
    let reranked_documents =
        u16::try_from(documents.len()).map_err(|_| RetrievalStageError::Failed)?;
    let report = RerankReport::applied(batch.model(), reranked_documents)
        .map_err(|_| RetrievalStageError::Failed)?;
    Ok((reranked, report))
}

fn apply_rerank_scores(
    channel: &ChannelResults,
    scores: &[f32],
) -> Result<ChannelResults, ProjectError> {
    if channel.channel() != RetrievalChannel::Semantic
        || channel.candidates().len() != scores.len()
        || scores.iter().any(|score| !score.is_finite())
    {
        return Err(ProjectError::RetrievalOperationFailed);
    }
    let mut order = scores.iter().copied().enumerate().collect::<Vec<_>>();
    order.sort_by(|(left_index, left_score), (right_index, right_score)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| {
                channel.candidates()[*left_index]
                    .rank()
                    .cmp(&channel.candidates()[*right_index].rank())
            })
            .then_with(|| {
                channel.candidates()[*left_index]
                    .document()
                    .document_id()
                    .cmp(channel.candidates()[*right_index].document().document_id())
            })
    });
    let candidates = order
        .into_iter()
        .enumerate()
        .map(|(index, (candidate_index, score))| {
            let rank = u16::try_from(index.saturating_add(1))
                .map_err(|_| ProjectError::RetrievalOperationFailed)?;
            ChannelCandidate::new(
                channel.candidates()[candidate_index].document().clone(),
                rank,
                f64::from(score),
            )
            .map_err(|_| ProjectError::RetrievalOperationFailed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    ChannelResults::new(RetrievalChannel::Semantic, candidates)
        .map(|results| results.with_truncated(channel.truncated()))
        .map_err(|_| ProjectError::RetrievalOperationFailed)
}

fn semantic_without_candidates(
    readiness: SemanticReadiness,
    reranker: &RerankClientState,
) -> Result<SemanticChannelResult, RetrievalStageError> {
    Ok(SemanticChannelResult {
        readiness,
        channel: None,
        rerank: rerank_report_without_candidates(reranker)?,
    })
}

fn rerank_report_without_candidates(
    reranker: &RerankClientState,
) -> Result<RerankReport, RetrievalStageError> {
    match reranker {
        RerankClientState::NotRequested => Ok(RerankReport::not_requested()),
        RerankClientState::NotConfigured => Ok(RerankReport::not_configured()),
        RerankClientState::Unavailable { model } => {
            RerankReport::unavailable(model.clone()).map_err(|_| RetrievalStageError::Failed)
        }
        RerankClientState::Ready(client) => RerankReport::skipped_no_candidates(client.model())
            .map_err(|_| RetrievalStageError::Failed),
    }
}

fn model_registration(
    client: &OpenAiEmbeddingClient,
    dimension: u16,
) -> Result<EmbeddingModelRegistration, SemanticStorageError> {
    let identity = client.identity();
    EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
        model_id: identity.model_id().clone(),
        fingerprint: identity.fingerprint().clone(),
        provider: MODEL_PROVIDER.to_owned(),
        model_name: identity.model().to_owned(),
        dimension,
        normalization: EmbeddingNormalization::None,
    })
}

fn semantic_results(hits: &[VectorSearchHit]) -> Result<ChannelResults, ProjectError> {
    let candidates = hits
        .iter()
        .enumerate()
        .map(|(index, hit)| semantic_candidate(hit, index))
        .collect::<Result<Vec<_>, _>>()?;
    ChannelResults::new(RetrievalChannel::Semantic, candidates)
        .map_err(|_| ProjectError::RetrievalOperationFailed)
}

fn semantic_candidate(
    hit: &VectorSearchHit,
    index: usize,
) -> Result<ChannelCandidate, ProjectError> {
    let rank = u16::try_from(index.saturating_add(1))
        .map_err(|_| ProjectError::RetrievalOperationFailed)?;
    let path =
        NormalizedPath::parse(hit.path()).map_err(|_| ProjectError::RetrievalOperationFailed)?;
    let language = SourceLanguage::from_stable_str(hit.language())
        .ok_or(ProjectError::RetrievalOperationFailed)?;
    let mut document = RetrievalDocument::new(RetrievalDocumentInput {
        document_id: hit.document_id().clone(),
        generation_id: hit.generation_id().clone(),
        path,
        language,
        document_kind: hit.document_kind(),
    });
    if let Some(file_id) = hit.file_id() {
        document = document.with_file_id(file_id.clone());
    }
    if let Some(symbol_id) = hit.symbol_id() {
        document = document.with_symbol_id(symbol_id.clone());
    }
    if let Some(symbol_kind) = hit.symbol_kind() {
        document = document.with_symbol_kind(symbol_kind);
    }
    let document = document
        .with_qualified_name(hit.qualified_name())
        .map_err(|_| ProjectError::RetrievalOperationFailed)?;
    let similarity = 1.0 - hit.distance();
    ChannelCandidate::new(document, rank, similarity)
        .map_err(|_| ProjectError::RetrievalOperationFailed)
}

fn optional_channel(channel: Option<ChannelResults>) -> Result<RetrievalChannels, ProjectError> {
    match channel {
        Some(channel) => RetrievalChannels::new()
            .with_channel(channel)
            .map_err(|_| ProjectError::RetrievalOperationFailed),
        None => Ok(RetrievalChannels::new()),
    }
}

async fn ensure_generation_current(
    runtime: &ProjectRuntime,
    project_id: &ProjectId,
    expected_generation_id: &GenerationId,
) -> Result<(), RetrievalStageError> {
    let current = runtime
        .database()
        .current_generation_record(project_id)
        .await
        .map_err(|error| storage_stage_error(&error))?;
    if current
        .as_ref()
        .is_some_and(|generation| generation.generation_id() == expected_generation_id)
    {
        Ok(())
    } else {
        Err(RetrievalStageError::GenerationChanged)
    }
}

fn storage_stage_error(error: &StorageError) -> RetrievalStageError {
    match error {
        StorageError::CurrentGenerationChanged => RetrievalStageError::GenerationChanged,
        _ => RetrievalStageError::Failed,
    }
}

fn retrieval_stage_error(error: &RetrievalError) -> RetrievalStageError {
    match error {
        RetrievalError::Storage(StorageError::CurrentGenerationChanged)
        | RetrievalError::Semantic(SemanticStorageError::CurrentGenerationChanged) => {
            RetrievalStageError::GenerationChanged
        }
        RetrievalError::InvalidInput { .. }
        | RetrievalError::Storage(_)
        | RetrievalError::Semantic(_) => RetrievalStageError::Failed,
    }
}

fn project_retrieval_error(error: RetrievalStageError) -> ProjectError {
    match error {
        RetrievalStageError::RequestCancelled => ProjectError::RequestCancelled,
        RetrievalStageError::GenerationChanged | RetrievalStageError::Failed => {
            ProjectError::RetrievalOperationFailed
        }
    }
}

/// Collapse database-specific semantic readiness evidence into the retrieval policy state shared
/// by agent and CLI callers.
#[must_use]
pub const fn semantic_readiness_from_database(state: SemanticReadinessState) -> SemanticReadiness {
    match state {
        SemanticReadinessState::Ready => SemanticReadiness::Ready,
        SemanticReadinessState::ModelMissing
        | SemanticReadinessState::NoCurrentGeneration
        | SemanticReadinessState::NoDocuments
        | SemanticReadinessState::CoverageIncomplete
        | SemanticReadinessState::HnswUnavailable => SemanticReadiness::NotIndexed,
        SemanticReadinessState::ModelMismatch | SemanticReadinessState::ModelRetired => {
            SemanticReadiness::Stale
        }
        SemanticReadinessState::QueryProbeFailed => SemanticReadiness::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cartograph_domain::{DocumentId, DocumentKind};
    use cartograph_search::RerankState;

    #[test]
    fn retrieval_options_default_to_auto_and_reject_unbounded_top_k() {
        assert_eq!(RetrievalOptions::default().mode(), SearchMode::Auto);
        assert_eq!(
            RetrievalOptions::default().candidate_limit(),
            DEFAULT_CANDIDATE_LIMIT
        );
        assert_eq!(
            RetrievalOptions::default().result_limit(),
            DEFAULT_CANDIDATE_LIMIT
        );
        assert_eq!(
            RetrievalOptions::new(SearchMode::Hybrid, 0),
            Err(ProjectError::InvalidOptions)
        );
        assert_eq!(
            RetrievalOptions::new(
                SearchMode::Hybrid,
                MAXIMUM_CANDIDATE_LIMIT.saturating_add(1)
            ),
            Err(ProjectError::InvalidOptions)
        );
    }

    #[test]
    fn deeper_channel_candidates_can_fuse_into_a_smaller_result_window() {
        let lexical = ChannelResults::new(
            RetrievalChannel::Lexical,
            vec![
                semantic_fixture_candidate("a", "src/a.rs", 1, 1.0),
                semantic_fixture_candidate("c", "src/c.rs", 2, 0.9),
                semantic_fixture_candidate("b", "src/b.rs", 3, 0.8),
            ],
        )
        .unwrap_or_else(|error| panic!("lexical fixture failed: {error}"));
        let semantic = ChannelResults::new(
            RetrievalChannel::Semantic,
            vec![semantic_fixture_candidate("b", "src/b.rs", 1, 0.95)],
        )
        .unwrap_or_else(|error| panic!("semantic fixture failed: {error}"));
        let options = RetrievalOptions::new(SearchMode::Hybrid, 3)
            .and_then(|options| options.with_result_limit(1))
            .unwrap_or_else(|error| panic!("separate retrieval limits failed: {error}"));
        let packet = PreparedRetrieval {
            semantic_readiness: SemanticReadiness::Ready,
            preference: RetrievalPreference::Neutral,
            channels: RetrievalChannels::new()
                .with_channel(lexical)
                .and_then(|channels| channels.with_channel(semantic))
                .unwrap_or_else(|error| panic!("retrieval channels failed: {error}")),
        }
        .into_packet(options)
        .unwrap_or_else(|error| panic!("retrieval fusion failed: {error}"));

        assert_eq!(options.candidate_limit(), 3);
        assert_eq!(options.result_limit(), 1);
        assert_eq!(packet.items().len(), 1);
        assert_eq!(packet.items()[0].document().path().as_str(), "src/b.rs");
        assert!(packet.truncated());
    }

    #[test]
    fn database_readiness_maps_to_explicit_agent_policy_states() {
        assert_eq!(
            semantic_readiness_from_database(SemanticReadinessState::Ready),
            SemanticReadiness::Ready
        );
        for state in [
            SemanticReadinessState::ModelMissing,
            SemanticReadinessState::NoCurrentGeneration,
            SemanticReadinessState::NoDocuments,
            SemanticReadinessState::CoverageIncomplete,
            SemanticReadinessState::HnswUnavailable,
        ] {
            assert_eq!(
                semantic_readiness_from_database(state),
                SemanticReadiness::NotIndexed
            );
        }
        for state in [
            SemanticReadinessState::ModelMismatch,
            SemanticReadinessState::ModelRetired,
        ] {
            assert_eq!(
                semantic_readiness_from_database(state),
                SemanticReadiness::Stale
            );
        }
        assert_eq!(
            semantic_readiness_from_database(SemanticReadinessState::QueryProbeFailed),
            SemanticReadiness::Unavailable
        );
    }

    #[test]
    fn rerank_scores_replace_only_semantic_order_with_stable_ties() {
        let channel = ChannelResults::new(
            RetrievalChannel::Semantic,
            vec![
                semantic_fixture_candidate("a", "src/a.rs", 1, 0.95),
                semantic_fixture_candidate("b", "src/b.rs", 2, 0.90),
                semantic_fixture_candidate("c", "src/c.rs", 3, 0.85),
            ],
        )
        .unwrap_or_else(|error| panic!("semantic fixture failed: {error}"));
        let reranked = apply_rerank_scores(&channel, &[0.1, 0.9, 0.9])
            .unwrap_or_else(|error| panic!("semantic rerank failed: {error}"));
        let paths = reranked
            .candidates()
            .iter()
            .map(|candidate| candidate.document().path().as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["src/b.rs", "src/c.rs", "src/a.rs"]);
        assert!(
            (reranked.candidates()[0].raw_score() - f64::from(0.9_f32)).abs() <= f64::EPSILON,
            "rerank score changed: {}",
            reranked.candidates()[0].raw_score()
        );
        assert_eq!(reranked.candidates()[0].rank(), 1);
        assert!(apply_rerank_scores(&channel, &[0.1]).is_err());
    }

    #[test]
    fn absent_reranker_has_an_explicit_non_model_outcome() {
        let report = rerank_report_without_candidates(&RerankClientState::NotConfigured)
            .unwrap_or_else(|_| panic!("not-configured rerank report failed"));
        assert_eq!(report.state(), RerankState::NotConfigured);
        assert_eq!(report.model(), None);
    }

    fn semantic_fixture_candidate(id: &str, path: &str, rank: u16, score: f64) -> ChannelCandidate {
        let document_id = DocumentId::parse(match id {
            "a" => "11111111-1111-4111-8111-111111111111",
            "b" => "22222222-2222-4222-8222-222222222222",
            "c" => "33333333-3333-4333-8333-333333333333",
            _ => panic!("unknown semantic fixture"),
        })
        .unwrap_or_else(|error| panic!("document fixture failed: {error}"));
        let generation_id = GenerationId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap_or_else(|error| panic!("generation fixture failed: {error}"));
        let path = NormalizedPath::parse(path)
            .unwrap_or_else(|error| panic!("path fixture failed: {error}"));
        let document = RetrievalDocument::new(RetrievalDocumentInput {
            document_id,
            generation_id,
            path,
            language: SourceLanguage::Rust,
            document_kind: DocumentKind::Symbol,
        });
        ChannelCandidate::new(document, rank, score)
            .unwrap_or_else(|error| panic!("candidate fixture failed: {error}"))
    }
}
