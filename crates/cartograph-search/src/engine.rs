use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use cartograph_db::{
    CartographDatabase, CurrentEntryPointsLookup, CurrentFileLookup, CurrentFileSymbolsLookup,
    CurrentFilesLookup, CurrentGenerationLookup, CurrentGraphLookup, CurrentReferenceRecord,
    CurrentSourceRangeLookup, CurrentSymbolRecord, CurrentSymbolSetLookup, EntryPointBucket,
    ExactTextLookup, GraphDirection, SearchComponent, SearchHit, SearchQuery, SemanticStorageError,
    SimilarSymbolsInput, SimilarSymbolsRequest, SimilarSymbolsResult, SourceLineRange,
};
use cartograph_domain::{
    DocumentKind, EdgeKind, FileId, GenerationId, NormalizedPath, ProjectId, SourceLanguage,
    SymbolId,
};

use crate::{
    AffectedTest, AffectedTestsResult, BidirectionalTraversalResult, ChannelCandidate,
    ChannelResults, ContextAnchor, ContextGraphDirection, ContextPacket, ContextRequest,
    EntryPointsQuery, EntryPointsResult, EvidenceItem, EvidenceReason, ExactPathQuery,
    ExactPathResult, ExactTextQuery, FileInventoryQuery, FileInventoryResult, GenerationEvidence,
    GraphPathRequest, GraphPathResult, GraphPathStep, HybridSearchInput, LexicalComponent,
    LexicalQuery, RetrievalChannel, RetrievalChannels, RetrievalDocument, RetrievalDocumentInput,
    RetrievalError, ReviewPacket, ReviewRequest, ReviewTruncation, SearchMode, SemanticReadiness,
    SimilarRequest, SourceRangeQuery, SourceRangeResult, TaskIntent, TraversalDirection,
    TraversalHop, TraversalNode, TraversalRequest, TraversalResult, fuse_search,
    model::{
        GraphPathResultInput, evidence_from_file, evidence_from_fused_item,
        evidence_from_reference, evidence_from_symbol, evidence_from_traversal_node,
    },
    packet::{PacketAssembly, assemble_packet},
    review::{ReviewAssembly, assemble_review_packet},
    traversal::{
        FrontierInput, GraphArc, affected_tests_from_nodes, affected_tests_from_traversal,
        expand_frontier, strongest_arc,
    },
};

const GRAPH_EDGE_READ_LIMIT: u16 = 2_000;
const MAX_CONTEXT_ROOTS: usize = 32;
const GENERATION_ATTEMPTS: usize = 2;
const SEMANTIC_READ_TIMEOUT: Duration = Duration::from_secs(30);
const CALLER_DIRECTIONS: [TraversalDirection; 1] = [TraversalDirection::Incoming];
const CALLEE_DIRECTIONS: [TraversalDirection; 1] = [TraversalDirection::Outgoing];
const BIDIRECTIONAL_DIRECTIONS: [TraversalDirection; 2] =
    [TraversalDirection::Incoming, TraversalDirection::Outgoing];

#[derive(Clone, Copy)]
enum TraversalKind {
    Calls,
    Impact,
}

/// Deterministic retrieval facade over one PostgreSQL-backed Cartograph schema.
#[derive(Clone)]
pub struct DeterministicRetriever {
    database: CartographDatabase,
}

struct ExactGenerationQuery<'project, Query> {
    project_id: &'project ProjectId,
    expected_generation_id: &'project GenerationId,
    query: Query,
}

struct LexicalAtInput<'query> {
    project_id: ProjectId,
    expected_generation_id: GenerationId,
    query: &'query LexicalQuery,
}

trait ExactTextRecordLookup {
    type Record;

    async fn lookup(
        database: &CartographDatabase,
        lookup: ExactTextLookup<'_>,
    ) -> Result<Vec<Self::Record>, cartograph_db::StorageError>;
}

struct ExactSymbolLookup;

impl ExactTextRecordLookup for ExactSymbolLookup {
    type Record = CurrentSymbolRecord;

    async fn lookup(
        database: &CartographDatabase,
        lookup: ExactTextLookup<'_>,
    ) -> Result<Vec<Self::Record>, cartograph_db::StorageError> {
        database.exact_current_symbols_by_name(lookup).await
    }
}

struct ExactReferenceLookup;

impl ExactTextRecordLookup for ExactReferenceLookup {
    type Record = CurrentReferenceRecord;

    async fn lookup(
        database: &CartographDatabase,
        lookup: ExactTextLookup<'_>,
    ) -> Result<Vec<Self::Record>, cartograph_db::StorageError> {
        database.exact_current_references_by_name(lookup).await
    }
}

#[derive(Clone, Copy)]
enum NamedSearchFlavor {
    Name,
    Intent,
}

struct NamedSearchRequest {
    project_id: ProjectId,
    query: LexicalQuery,
    flavor: NamedSearchFlavor,
}

impl NamedSearchRequest {
    const fn new(project_id: ProjectId, query: LexicalQuery, flavor: NamedSearchFlavor) -> Self {
        Self {
            project_id,
            query,
            flavor,
        }
    }
}

/// Current-generation fuzzy name query with an explicit edit distance.
pub struct FuzzyNameRequest {
    project_id: ProjectId,
    query: LexicalQuery,
    edit_distance: u8,
}

impl FuzzyNameRequest {
    #[must_use]
    /// Creates a validated fuzzy name request.
    pub const fn new(project_id: ProjectId, query: LexicalQuery, edit_distance: u8) -> Self {
        Self {
            project_id,
            query,
            edit_distance,
        }
    }
}

/// BM25 channel query fenced to one caller-observed generation.
pub struct GenerationLexicalRequest {
    project_id: ProjectId,
    expected_generation_id: GenerationId,
    query: LexicalQuery,
}

impl GenerationLexicalRequest {
    #[must_use]
    /// Creates a validated generation lexical request.
    pub const fn new(
        project_id: ProjectId,
        expected_generation_id: GenerationId,
        query: LexicalQuery,
    ) -> Self {
        Self {
            project_id,
            expected_generation_id,
            query,
        }
    }
}

struct TraversalRetryInput<'request> {
    request: &'request TraversalRequest,
    kind: TraversalKind,
    direction: TraversalDirection,
}

impl DeterministicRetriever {
    /// Bind deterministic retrieval to an established database handle.
    #[must_use]
    pub const fn new(database: CartographDatabase) -> Self {
        Self { database }
    }

    /// Exact fully qualified declaration-name lookup in the published generation.
    /// # Errors
    ///
    /// Returns an error if the current-generation lookup fails or the database
    /// cannot decode the exact-name symbol records.
    pub async fn exact_name(
        &self,
        project_id: &ProjectId,
        query: ExactTextQuery<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, RetrievalError> {
        exact_current_records::<ExactSymbolLookup>(&self.database, project_id, query).await
    }

    async fn exact_name_at(
        &self,
        input: ExactGenerationQuery<'_, ExactTextQuery<'_>>,
    ) -> Result<Vec<CurrentSymbolRecord>, RetrievalError> {
        exact_records_at::<ExactSymbolLookup>(&self.database, input).await
    }

    /// Exact canonical-path lookup with source-ordered declarations.
    /// # Errors
    ///
    /// Returns an error if the generation, file, or source-ordered symbol read
    /// fails while resolving the canonical path.
    pub async fn exact_path(
        &self,
        project_id: &ProjectId,
        query: ExactPathQuery<'_>,
    ) -> Result<Option<ExactPathResult>, RetrievalError> {
        let Some(generation) = self.database.current_generation_record(project_id).await? else {
            return Ok(None);
        };
        self.exact_path_at(ExactGenerationQuery {
            project_id,
            expected_generation_id: generation.generation_id(),
            query,
        })
        .await
    }

    async fn exact_path_at(
        &self,
        input: ExactGenerationQuery<'_, ExactPathQuery<'_>>,
    ) -> Result<Option<ExactPathResult>, RetrievalError> {
        let Some(file) = self
            .database
            .exact_current_file_by_path(CurrentFileLookup::new(
                input.project_id,
                input.expected_generation_id,
                input.query.path(),
            ))
            .await?
        else {
            return Ok(None);
        };
        let symbols = self
            .database
            .current_symbols_by_file(CurrentFileSymbolsLookup::new(
                CurrentGenerationLookup::new(input.project_id, input.expected_generation_id),
                file.file_id(),
                input.query.symbol_limit(),
            ))
            .await?;
        Ok(Some(ExactPathResult::new(file, symbols)))
    }

    /// List a bounded current-generation file inventory with optional directory/language filters.
    /// # Errors
    ///
    /// Returns an error if the inventory filters cannot be applied to a stable
    /// current generation or the backing file query fails.
    pub async fn files(
        &self,
        project_id: &ProjectId,
        query: &FileInventoryQuery,
    ) -> Result<FileInventoryResult, RetrievalError> {
        retrieve_files(&self.database, project_id, query).await
    }

    /// Discover typed routes, commands, MCP tools, CLI declarations, and public API boundaries.
    /// # Errors
    ///
    /// Returns an error if entry-point evidence cannot be read from one stable
    /// generation or a returned bucket violates its bounds.
    pub async fn entry_points(
        &self,
        project_id: &ProjectId,
        query: EntryPointsQuery,
    ) -> Result<EntryPointsResult, RetrievalError> {
        retrieve_entry_points(&self.database, project_id, query).await
    }

    /// Resolve the smallest current-generation symbols overlapping one exact source range.
    /// # Errors
    ///
    /// Returns an error if the file/range lookup fails, the generation changes
    /// during the read, or overlapping symbols cannot be decoded.
    pub async fn symbols_at_range(
        &self,
        project_id: &ProjectId,
        query: &SourceRangeQuery,
    ) -> Result<Option<SourceRangeResult>, RetrievalError> {
        retrieve_symbols_at_range(&self.database, project_id, query).await
    }

    /// Find a bounded shortest outgoing dependency path under one generation fence.
    /// # Errors
    ///
    /// Returns an error if either endpoint is unavailable, a graph read fails,
    /// or the generation changes while assembling the bounded path.
    pub async fn path(
        &self,
        request: &GraphPathRequest,
    ) -> Result<GraphPathResult, RetrievalError> {
        retrieve_graph_path(&self.database, request).await
    }

    /// Find model-scoped symbol neighbors from a stored current-generation vector.
    /// # Errors
    ///
    /// Returns an error if semantic readiness fails, the requested model or
    /// source symbol is unavailable, or the bounded vector query times out.
    pub async fn similar(
        &self,
        request: &SimilarRequest,
    ) -> Result<SimilarSymbolsResult, RetrievalError> {
        retrieve_similar_symbols(&self.database, request).await
    }

    /// Exact source-reference lookup, including unresolved reference evidence.
    /// # Errors
    ///
    /// Returns an error if the current-generation lookup fails or the database
    /// cannot decode the exact reference records.
    pub async fn exact_reference(
        &self,
        project_id: &ProjectId,
        query: ExactTextQuery<'_>,
    ) -> Result<Vec<CurrentReferenceRecord>, RetrievalError> {
        exact_current_records::<ExactReferenceLookup>(&self.database, project_id, query).await
    }

    async fn exact_reference_at(
        &self,
        input: ExactGenerationQuery<'_, ExactTextQuery<'_>>,
    ) -> Result<Vec<CurrentReferenceRecord>, RetrievalError> {
        exact_records_at::<ExactReferenceLookup>(&self.database, input).await
    }

    /// Current-generation `ParadeDB` BM25 with ordered field provenance.
    /// # Errors
    ///
    /// Returns an error if the current generation cannot be read or `ParadeDB`
    /// rejects or cannot execute the bounded code query.
    pub async fn bm25(
        &self,
        project_id: ProjectId,
        query: LexicalQuery,
    ) -> Result<Vec<SearchHit>, RetrievalError> {
        let Some(generation) = self.database.current_generation_record(&project_id).await? else {
            return Ok(Vec::new());
        };
        self.bm25_at(LexicalAtInput {
            project_id,
            expected_generation_id: generation.generation_id().clone(),
            query: &query,
        })
        .await
    }

    /// Current-generation `ParadeDB` fuzzy name search with explicit edit distance.
    /// # Errors
    ///
    /// Returns an error if the current generation cannot be read or `ParadeDB`
    /// cannot execute the requested bounded edit-distance search.
    pub async fn fuzzy_name(
        &self,
        request: FuzzyNameRequest,
    ) -> Result<Vec<SearchHit>, RetrievalError> {
        let Some(generation) = self
            .database
            .current_generation_record(&request.project_id)
            .await?
        else {
            return Ok(Vec::new());
        };
        self.database
            .search_current_names_fuzzy(
                SearchQuery::new(
                    CurrentGenerationLookup::new(&request.project_id, generation.generation_id()),
                    request.query.query(),
                    request.query.limit(),
                ),
                request.edit_distance,
            )
            .await
            .map_err(Into::into)
    }

    /// Current-generation `ParadeDB` name-only search.
    /// # Errors
    ///
    /// Returns an error if the current generation cannot be read or `ParadeDB`
    /// cannot execute the bounded name-field query.
    pub async fn name(
        &self,
        project_id: ProjectId,
        query: LexicalQuery,
    ) -> Result<Vec<SearchHit>, RetrievalError> {
        retrieve_named_hits(
            &self.database,
            NamedSearchRequest::new(project_id, query, NamedSearchFlavor::Name),
        )
        .await
    }

    /// Current-generation `ParadeDB` natural-language intent search.
    /// # Errors
    ///
    /// Returns an error if the current generation cannot be read or `ParadeDB`
    /// cannot execute the natural-language intent query.
    pub async fn intent(
        &self,
        project_id: ProjectId,
        query: LexicalQuery,
    ) -> Result<Vec<SearchHit>, RetrievalError> {
        retrieve_named_hits(
            &self.database,
            NamedSearchRequest::new(project_id, query, NamedSearchFlavor::Intent),
        )
        .await
    }

    async fn bm25_at(&self, input: LexicalAtInput<'_>) -> Result<Vec<SearchHit>, RetrievalError> {
        self.database
            .search_current_code(SearchQuery::new(
                CurrentGenerationLookup::new(&input.project_id, &input.expected_generation_id),
                input.query.query(),
                input.query.limit(),
            ))
            .await
            .map_err(Into::into)
    }

    /// Adapt current BM25 results into a channel that can run concurrently with semantics.
    /// # Errors
    ///
    /// Returns an error if BM25 retrieval fails or a returned hit cannot be
    /// represented as a bounded lexical channel candidate.
    pub async fn lexical_channel(
        &self,
        project_id: ProjectId,
        query: LexicalQuery,
    ) -> Result<ChannelResults, RetrievalError> {
        let hits = self.bm25(project_id, query).await?;
        lexical_results(&hits)
    }

    /// Adapt BM25 results while fencing the read to one caller-observed generation.
    /// # Errors
    ///
    /// Returns an error if the fenced BM25 read fails or a returned hit cannot
    /// be represented as a bounded lexical channel candidate.
    pub async fn lexical_channel_for_generation(
        &self,
        request: GenerationLexicalRequest,
    ) -> Result<ChannelResults, RetrievalError> {
        let hits = self
            .bm25_at(LexicalAtInput {
                project_id: request.project_id,
                expected_generation_id: request.expected_generation_id,
                query: &request.query,
            })
            .await?;
        lexical_results(&hits)
    }

    /// Follow incoming `calls` edges to discover bounded callers.
    /// # Errors
    ///
    /// Returns an error if the root or current generation is unavailable, a
    /// caller-edge read fails, or the generation changes during traversal.
    pub async fn callers(
        &self,
        request: &TraversalRequest,
    ) -> Result<TraversalResult, RetrievalError> {
        self.traverse_with_retry(TraversalRetryInput {
            request,
            kind: TraversalKind::Calls,
            direction: TraversalDirection::Incoming,
        })
        .await
    }

    /// Follow outgoing `calls` edges to discover bounded callees.
    /// # Errors
    ///
    /// Returns an error if the root or current generation is unavailable, a
    /// callee-edge read fails, or the generation changes during traversal.
    pub async fn callees(
        &self,
        request: &TraversalRequest,
    ) -> Result<TraversalResult, RetrievalError> {
        self.traverse_with_retry(TraversalRetryInput {
            request,
            kind: TraversalKind::Calls,
            direction: TraversalDirection::Outgoing,
        })
        .await
    }

    /// Follow incoming and outgoing dependency edges under one generation fence.
    /// # Errors
    ///
    /// Returns an error if either directional traversal fails or both results
    /// cannot be reconciled under the same generation fence.
    pub async fn both(
        &self,
        request: &TraversalRequest,
    ) -> Result<BidirectionalTraversalResult, RetrievalError> {
        retrieve_bidirectional_traversal(&self.database, request).await
    }

    /// Follow incoming dependency relations to estimate a bounded impact cone.
    /// # Errors
    ///
    /// Returns an error if the root or current generation is unavailable, an
    /// incoming dependency read fails, or the generation changes mid-query.
    pub async fn impact(
        &self,
        request: &TraversalRequest,
    ) -> Result<TraversalResult, RetrievalError> {
        self.traverse_with_retry(TraversalRetryInput {
            request,
            kind: TraversalKind::Impact,
            direction: TraversalDirection::Incoming,
        })
        .await
    }

    /// Discover test files/symbols in a bounded reverse impact cone.
    /// # Errors
    ///
    /// Returns an error if `limit` is invalid, reverse traversal fails, or the
    /// discovered test evidence cannot be bounded under one generation.
    pub async fn affected_tests(
        &self,
        request: &TraversalRequest,
        limit: u16,
    ) -> Result<AffectedTestsResult, RetrievalError> {
        retrieve_affected_tests(self, request, limit).await
    }

    /// Assemble compact exact, BM25, graph, and affected-test evidence without
    /// invoking any model or external service.
    /// # Errors
    ///
    /// Returns an error if an exact, lexical, graph, or affected-test read
    /// fails, or packet evidence violates the request's hard bounds.
    pub async fn context_packet(
        &self,
        request: &ContextRequest,
    ) -> Result<ContextPacket, RetrievalError> {
        build_context_packet_with_retry(self, request, None).await
    }

    /// Assemble exact/graph/test evidence around caller-precomputed lexical and semantic channels.
    /// # Errors
    ///
    /// Returns an error if the supplied channels are inconsistent or an exact,
    /// graph, affected-test, or packet-assembly operation fails.
    pub async fn context_packet_with_channels(
        &self,
        request: &ContextRequest,
        channels: RetrievalChannels,
    ) -> Result<ContextPacket, RetrievalError> {
        build_context_packet_with_retry(self, request, Some(channels)).await
    }

    /// Assemble exact changed-file, reverse-impact, and affected-test evidence
    /// for a deterministic compare-to-ref workflow.
    /// # Errors
    ///
    /// Returns an error if changed-file lookup, reverse-impact traversal,
    /// affected-test discovery, or bounded review assembly fails.
    pub async fn review_packet(
        &self,
        request: &ReviewRequest,
    ) -> Result<ReviewPacket, RetrievalError> {
        build_review_packet_with_retry(self, request).await
    }

    async fn traverse_with_retry(
        &self,
        input: TraversalRetryInput<'_>,
    ) -> Result<TraversalResult, RetrievalError> {
        retrieve_traversal_with_retry(&self.database, input).await
    }
}

async fn exact_current_records<Lookup>(
    database: &CartographDatabase,
    project_id: &ProjectId,
    query: ExactTextQuery<'_>,
) -> Result<Vec<Lookup::Record>, RetrievalError>
where
    Lookup: ExactTextRecordLookup,
{
    let Some(generation) = database.current_generation_record(project_id).await? else {
        return Ok(Vec::new());
    };
    exact_records_at::<Lookup>(
        database,
        ExactGenerationQuery {
            project_id,
            expected_generation_id: generation.generation_id(),
            query,
        },
    )
    .await
}

async fn exact_records_at<Lookup>(
    database: &CartographDatabase,
    input: ExactGenerationQuery<'_, ExactTextQuery<'_>>,
) -> Result<Vec<Lookup::Record>, RetrievalError>
where
    Lookup: ExactTextRecordLookup,
{
    Lookup::lookup(
        database,
        ExactTextLookup::new(
            CurrentGenerationLookup::new(input.project_id, input.expected_generation_id),
            input.query.value(),
            input.query.limit(),
        ),
    )
    .await
    .map_err(Into::into)
}

async fn retrieve_named_hits(
    database: &CartographDatabase,
    request: NamedSearchRequest,
) -> Result<Vec<SearchHit>, RetrievalError> {
    let Some(generation) = database
        .current_generation_record(&request.project_id)
        .await?
    else {
        return Ok(Vec::new());
    };
    let query = SearchQuery::new(
        CurrentGenerationLookup::new(&request.project_id, generation.generation_id()),
        request.query.query(),
        request.query.limit(),
    );
    match request.flavor {
        NamedSearchFlavor::Name => database.search_current_names(query).await,
        NamedSearchFlavor::Intent => database.search_current_intent(query).await,
    }
    .map_err(Into::into)
}

async fn retrieve_files(
    database: &CartographDatabase,
    project_id: &ProjectId,
    query: &FileInventoryQuery,
) -> Result<FileInventoryResult, RetrievalError> {
    let Some(generation) = database.current_generation_record(project_id).await? else {
        return Ok(FileInventoryResult::new(Vec::new(), false));
    };
    let fetch_limit = query.limit().saturating_add(1);
    let mut request = CurrentFilesLookup::new(project_id, generation.generation_id(), fetch_limit);
    if let Some(directory) = query.directory() {
        request = request.within_directory(directory);
    }
    if let Some(language) = query.language() {
        request = request.with_language(language);
    }
    let mut files = database.current_files(request).await?;
    let truncated = files.len() > usize::from(query.limit());
    files.truncate(usize::from(query.limit()));
    Ok(FileInventoryResult::new(files, truncated))
}

async fn retrieve_entry_points(
    database: &CartographDatabase,
    project_id: &ProjectId,
    query: EntryPointsQuery,
) -> Result<EntryPointsResult, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        let Some(generation) = database.current_generation_record(project_id).await? else {
            return Ok(EntryPointsResult::new(None, Vec::new()));
        };
        let selected = query.bucket();
        let mut pages = Vec::with_capacity(if selected.is_some() { 1 } else { 5 });
        let mut retry = false;
        for bucket in EntryPointBucket::ALL {
            if selected.is_some_and(|selected| selected != bucket) {
                continue;
            }
            let page = database
                .current_entry_points(CurrentEntryPointsLookup::new(
                    CurrentGenerationLookup::new(project_id, generation.generation_id()),
                    bucket,
                    query.limit(),
                ))
                .await;
            match page {
                Err(error) if attempt == 0 && is_generation_changed_storage(&error) => {
                    retry = true;
                    break;
                }
                Err(error) => return Err(error.into()),
                Ok(page) => pages.push(page),
            }
        }
        if retry {
            continue;
        }
        return Ok(EntryPointsResult::new(
            Some(generation.generation_id().clone()),
            pages,
        ));
    }
    Err(cartograph_db::StorageError::CurrentGenerationChanged.into())
}

async fn retrieve_symbols_at_range(
    database: &CartographDatabase,
    project_id: &ProjectId,
    query: &SourceRangeQuery,
) -> Result<Option<SourceRangeResult>, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        let Some(generation) = database.current_generation_record(project_id).await? else {
            return Ok(None);
        };
        let file = database
            .exact_current_file_by_path(CurrentFileLookup::new(
                project_id,
                generation.generation_id(),
                query.path(),
            ))
            .await;
        let file = match file {
            Err(error) if attempt == 0 && is_generation_changed_storage(&error) => continue,
            Err(error) => return Err(error.into()),
            Ok(None) => return Ok(None),
            Ok(Some(file)) => file,
        };
        let fetch_limit = query.limit().saturating_add(1);
        let symbols = database
            .current_symbols_at_range(CurrentSourceRangeLookup::new(
                CurrentGenerationLookup::new(project_id, generation.generation_id()),
                SourceLineRange::new(query.path(), query.start_line(), query.end_line()),
                fetch_limit,
            ))
            .await;
        let mut symbols = match symbols {
            Err(error) if attempt == 0 && is_generation_changed_storage(&error) => continue,
            Err(error) => return Err(error.into()),
            Ok(symbols) => symbols,
        };
        let truncated = symbols.len() > usize::from(query.limit());
        symbols.truncate(usize::from(query.limit()));
        return Ok(Some(SourceRangeResult::new(file, symbols, truncated)));
    }
    Err(cartograph_db::StorageError::CurrentGenerationChanged.into())
}

async fn retrieve_graph_path(
    database: &CartographDatabase,
    request: &GraphPathRequest,
) -> Result<GraphPathResult, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        let Some(generation) = database
            .current_generation_record(request.project_id())
            .await?
        else {
            return Ok(GraphPathResult::new(GraphPathResultInput {
                start: request.start().clone(),
                target: request.target().clone(),
                path: None,
                truncated: false,
            }));
        };
        match run_graph_path(database, request, generation.generation_id()).await {
            Err(error) if attempt == 0 && is_generation_changed(&error) => {}
            result => return result,
        }
    }
    Err(cartograph_db::StorageError::CurrentGenerationChanged.into())
}

async fn retrieve_similar_symbols(
    database: &CartographDatabase,
    request: &SimilarRequest,
) -> Result<SimilarSymbolsResult, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        let generation = database
            .current_generation_record(request.project_id())
            .await?
            .ok_or(SemanticStorageError::CurrentGenerationUnavailable)?;
        let database_request = SimilarSymbolsRequest::new(SimilarSymbolsInput {
            project_id: request.project_id().clone(),
            expected_generation_id: generation.generation_id().clone(),
            source_symbol_id: request.source_symbol_id().clone(),
            model_id: request.model_id().cloned(),
            limit: request.limit(),
            minimum_score: request.minimum_score(),
            same_language: request.same_language(),
            statement_timeout: SEMANTIC_READ_TIMEOUT,
        })?;
        match database.similar_current_symbols(database_request).await {
            Err(SemanticStorageError::CurrentGenerationChanged) if attempt == 0 => {}
            Err(error) => return Err(error.into()),
            Ok(result) => return Ok(result),
        }
    }
    Err(SemanticStorageError::CurrentGenerationChanged.into())
}

async fn retrieve_bidirectional_traversal(
    database: &CartographDatabase,
    request: &TraversalRequest,
) -> Result<BidirectionalTraversalResult, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        let Some(generation) = database
            .current_generation_record(request.project_id())
            .await?
        else {
            return Ok(BidirectionalTraversalResult::new(
                empty_traversal(request, TraversalDirection::Incoming),
                empty_traversal(request, TraversalDirection::Outgoing),
            ));
        };
        let traversals = tokio::try_join!(
            run_traversal(
                database,
                TraversalExecution {
                    request,
                    expected_generation_id: generation.generation_id(),
                    plan: traversal_plan(TraversalKind::Impact, TraversalDirection::Incoming),
                },
            ),
            run_traversal(
                database,
                TraversalExecution {
                    request,
                    expected_generation_id: generation.generation_id(),
                    plan: traversal_plan(TraversalKind::Impact, TraversalDirection::Outgoing),
                },
            ),
        );
        match traversals {
            Err(error) if attempt == 0 && is_generation_changed(&error) => {}
            Err(error) => return Err(error),
            Ok((incoming, outgoing)) => {
                return Ok(BidirectionalTraversalResult::new(incoming, outgoing));
            }
        }
    }
    Err(cartograph_db::StorageError::CurrentGenerationChanged.into())
}

async fn retrieve_affected_tests(
    retriever: &DeterministicRetriever,
    request: &TraversalRequest,
    limit: u16,
) -> Result<AffectedTestsResult, RetrievalError> {
    if limit == 0 || limit > 500 {
        return Err(RetrievalError::InvalidInput {
            field: "affected_test_limit",
        });
    }
    let impact = retriever.impact(request).await?;
    let (tests, output_was_truncated) = affected_tests_from_traversal(&impact, limit);
    Ok(AffectedTestsResult::new(
        tests,
        impact.truncated() || output_was_truncated,
    ))
}

async fn retrieve_traversal_with_retry(
    database: &CartographDatabase,
    input: TraversalRetryInput<'_>,
) -> Result<TraversalResult, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        let Some(generation) = database
            .current_generation_record(input.request.project_id())
            .await?
        else {
            return Ok(empty_traversal(input.request, input.direction));
        };
        match run_traversal(
            database,
            TraversalExecution {
                request: input.request,
                expected_generation_id: generation.generation_id(),
                plan: traversal_plan(input.kind, input.direction),
            },
        )
        .await
        {
            Err(error) if attempt == 0 && is_generation_changed(&error) => {}
            result => return result,
        }
    }
    Err(cartograph_db::StorageError::CurrentGenerationChanged.into())
}

fn empty_traversal(request: &TraversalRequest, direction: TraversalDirection) -> TraversalResult {
    TraversalResult {
        direction,
        roots: request.roots().to_vec(),
        nodes: Vec::new(),
        truncated: false,
    }
}

fn is_generation_changed(error: &RetrievalError) -> bool {
    matches!(
        error,
        RetrievalError::Storage(cartograph_db::StorageError::CurrentGenerationChanged)
    )
}

fn is_generation_changed_storage(error: &cartograph_db::StorageError) -> bool {
    matches!(error, cartograph_db::StorageError::CurrentGenerationChanged)
}

#[derive(Default)]
struct ContextBuildState {
    evidence: Vec<EvidenceItem>,
    roots: OrderedRoots,
    seen_reference_ids: BTreeSet<u64>,
}

#[derive(Default)]
struct OrderedRoots {
    values: Vec<SymbolId>,
    seen: BTreeSet<SymbolId>,
}

impl OrderedRoots {
    fn insert(&mut self, symbol_id: SymbolId) {
        if self.seen.insert(symbol_id.clone()) {
            self.values.push(symbol_id);
        }
    }

    fn into_values(self) -> Vec<SymbolId> {
        self.values
    }
}

struct ContextExpansionInput<'request, 'generation, 'evidence> {
    request: &'request ContextRequest,
    expected_generation_id: &'generation GenerationId,
    roots: Vec<SymbolId>,
    evidence: &'evidence mut Vec<EvidenceItem>,
    direct_tests: DirectTestResult,
}

struct ContextGraphResult {
    affected_tests: Vec<AffectedTest>,
    truncated: bool,
}

#[derive(Default)]
struct DirectTestResult {
    tests: Vec<AffectedTest>,
    truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirectTestCandidate {
    Symbol(SymbolId),
    File(FileId),
}

#[derive(Default)]
struct ReviewBuildState {
    indexed_changed_files: Vec<cartograph_domain::NormalizedPath>,
    evidence: Vec<EvidenceItem>,
    roots: BTreeSet<SymbolId>,
    symbol_roots_truncated: bool,
}

struct ReviewExpansionInput<'request, 'generation, 'evidence> {
    project_id: &'request ProjectId,
    expected_generation_id: &'generation GenerationId,
    request: &'request ReviewRequest,
    roots: Vec<SymbolId>,
    evidence: &'evidence mut Vec<EvidenceItem>,
}

struct ReviewGraphResult {
    affected_tests: Vec<AffectedTest>,
    graph_truncated: bool,
    affected_tests_truncated: bool,
}

#[derive(Clone, Copy)]
struct TraversalPlan {
    kind: TraversalKind,
    direction: TraversalDirection,
    database_direction: GraphDirection,
}

#[derive(Clone, Copy)]
struct TraversalExecution<'request, 'generation> {
    request: &'request TraversalRequest,
    expected_generation_id: &'generation GenerationId,
    plan: TraversalPlan,
}

struct TraversalHydrationInput<'request, 'generation> {
    execution: TraversalExecution<'request, 'generation>,
    discovery: GraphDiscovery,
}

struct GraphPathHydrationInput<'request, 'generation> {
    request: &'request GraphPathRequest,
    expected_generation_id: &'generation GenerationId,
    path: Vec<(SymbolId, Option<GraphArc>)>,
    truncated: bool,
}

#[derive(Clone, Copy)]
struct ContextGenerationInput<'request, 'generation> {
    request: &'request ContextRequest,
    expected_generation_id: &'generation GenerationId,
}

#[derive(Clone, Copy)]
struct ContextTraversalsInput<'request, 'generation> {
    request: &'request TraversalRequest,
    expected_generation_id: &'generation GenerationId,
    kind: TraversalKind,
    direction: ContextGraphDirection,
}

#[derive(Clone, Copy)]
struct ReviewGenerationInput<'request, 'generation> {
    project_id: &'request ProjectId,
    expected_generation_id: &'generation GenerationId,
    request: &'request ReviewRequest,
}

struct GraphDiscovery {
    direction: TraversalDirection,
    discoveries: BTreeMap<SymbolId, (u8, GraphArc)>,
    truncated: bool,
}

fn lexical_results(hits: &[SearchHit]) -> Result<ChannelResults, RetrievalError> {
    let candidates = hits
        .iter()
        .enumerate()
        .map(|(index, hit)| lexical_candidate(hit, index))
        .collect::<Result<Vec<_>, _>>()?;
    ChannelResults::new(RetrievalChannel::Lexical, candidates)
}

fn lexical_candidate(hit: &SearchHit, index: usize) -> Result<ChannelCandidate, RetrievalError> {
    let rank =
        u16::try_from(index.saturating_add(1)).map_err(|_| RetrievalError::InvalidInput {
            field: "candidate_limit",
        })?;
    let path = NormalizedPath::parse(hit.path()).map_err(|_| RetrievalError::InvalidInput {
        field: "candidate_path",
    })?;
    let language =
        SourceLanguage::from_stable_str(hit.language()).ok_or(RetrievalError::InvalidInput {
            field: "candidate_language",
        })?;
    let document_kind = parse_document_kind(hit.document_kind())?;
    let mut document = RetrievalDocument::new(RetrievalDocumentInput {
        document_id: hit.document_id().clone(),
        generation_id: hit.generation_id().clone(),
        path,
        language,
        document_kind,
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
    let document = document.with_qualified_name(hit.qualified_name())?;
    let components = hit
        .components()
        .iter()
        .copied()
        .map(lexical_component)
        .collect();
    Ok(ChannelCandidate::new(document, rank, hit.score())?.with_lexical_components(components))
}

fn parse_document_kind(value: &str) -> Result<DocumentKind, RetrievalError> {
    match value {
        "symbol" => Ok(DocumentKind::Symbol),
        "file" => Ok(DocumentKind::File),
        "documentation" => Ok(DocumentKind::Documentation),
        "test" => Ok(DocumentKind::Test),
        "configuration" => Ok(DocumentKind::Configuration),
        _ => Err(RetrievalError::InvalidInput {
            field: "candidate_document_kind",
        }),
    }
}

const fn lexical_component(component: SearchComponent) -> LexicalComponent {
    match component {
        SearchComponent::QualifiedName => LexicalComponent::QualifiedName,
        SearchComponent::Code => LexicalComponent::Code,
        SearchComponent::NaturalText => LexicalComponent::NaturalText,
    }
}

#[derive(Clone, Copy)]
struct DiscoveryStep<'expansion> {
    depth: u8,
    direction: TraversalDirection,
    expansion: &'expansion crate::traversal::FrontierExpansion,
}

async fn run_graph_path(
    database: &CartographDatabase,
    request: &GraphPathRequest,
    expected_generation_id: &GenerationId,
) -> Result<GraphPathResult, RetrievalError> {
    require_graph_path_endpoints(database, request, expected_generation_id).await?;
    if request.start() == request.target() {
        return hydrate_graph_path(
            database,
            GraphPathHydrationInput {
                request,
                expected_generation_id,
                path: vec![(request.start().clone(), None)],
                truncated: false,
            },
        )
        .await;
    }

    let budget = request.budget();
    let mut visited = BTreeSet::from([request.start().clone()]);
    let mut frontier = vec![request.start().clone()];
    let mut parents = BTreeMap::<SymbolId, GraphArc>::new();
    let mut truncated = false;
    for depth in 1..=budget.max_depth() {
        let edges = database
            .current_graph_edges(
                CurrentGraphLookup::new(
                    CurrentGenerationLookup::new(request.project_id(), expected_generation_id),
                    &frontier,
                    GraphDirection::Outgoing,
                )
                .with_limit(GRAPH_EDGE_READ_LIMIT),
            )
            .await?;
        truncated |= edges.len() == usize::from(GRAPH_EDGE_READ_LIMIT);
        let arcs = edges
            .iter()
            .map(GraphArc::from_record)
            .filter(|arc| arc.confidence >= request.minimum_confidence())
            .filter(|arc| {
                request
                    .edge_kind()
                    .is_none_or(|edge_kind| arc.edge_kind == edge_kind.as_str())
            })
            .filter(|arc| !visited.contains(&arc.target))
            .collect::<Vec<_>>();
        let remaining = usize::from(budget.max_nodes()).saturating_sub(parents.len());
        if remaining == 0 {
            truncated |= !arcs.is_empty();
            break;
        }
        let expansion = expand_frontier(FrontierInput {
            frontier: &frontier,
            arcs: &arcs,
            direction: TraversalDirection::Outgoing,
            max_new_nodes: remaining,
        });
        truncated |= expansion.truncated;
        record_path_parents(&expansion, &mut visited, &mut parents);
        if visited.contains(request.target()) {
            let path = reconstruct_graph_path(request, &parents)?;
            return hydrate_graph_path(
                database,
                GraphPathHydrationInput {
                    request,
                    expected_generation_id,
                    path,
                    truncated,
                },
            )
            .await;
        }
        frontier = expansion.next;
        if frontier.is_empty() {
            break;
        }
        if depth == budget.max_depth() {
            truncated = true;
        }
    }
    Ok(GraphPathResult::new(GraphPathResultInput {
        start: request.start().clone(),
        target: request.target().clone(),
        path: None,
        truncated,
    }))
}

async fn require_graph_path_endpoints(
    database: &CartographDatabase,
    request: &GraphPathRequest,
    expected_generation_id: &GenerationId,
) -> Result<(), RetrievalError> {
    let mut ids = vec![request.start().clone()];
    if request.target() != request.start() {
        ids.push(request.target().clone());
    }
    let found = database
        .current_symbols_by_ids(CurrentSymbolSetLookup::new(
            request.project_id(),
            expected_generation_id,
            &ids,
        ))
        .await?;
    if found.len() != ids.len() {
        return Err(RetrievalError::InvalidInput {
            field: "path_endpoint",
        });
    }
    Ok(())
}

fn record_path_parents(
    expansion: &crate::traversal::FrontierExpansion,
    visited: &mut BTreeSet<SymbolId>,
    parents: &mut BTreeMap<SymbolId, GraphArc>,
) {
    for symbol_id in &expansion.next {
        let Some(best_arc) =
            strongest_arc(&expansion.arcs, symbol_id, TraversalDirection::Outgoing)
        else {
            continue;
        };
        visited.insert(symbol_id.clone());
        parents.insert(symbol_id.clone(), best_arc.clone());
    }
}

fn reconstruct_graph_path(
    request: &GraphPathRequest,
    parents: &BTreeMap<SymbolId, GraphArc>,
) -> Result<Vec<(SymbolId, Option<GraphArc>)>, RetrievalError> {
    let mut reversed = Vec::new();
    let mut current = request.target().clone();
    while &current != request.start() {
        if reversed.len() > parents.len() {
            return Err(cartograph_db::StorageError::CorruptStoredValue {
                field: "graph_path",
            }
            .into());
        }
        let arc = parents.get(&current).cloned().ok_or(
            cartograph_db::StorageError::CorruptStoredValue {
                field: "graph_path",
            },
        )?;
        reversed.push((current, Some(arc.clone())));
        current = arc.source;
    }
    reversed.push((request.start().clone(), None));
    reversed.reverse();
    Ok(reversed)
}

async fn hydrate_graph_path(
    database: &CartographDatabase,
    input: GraphPathHydrationInput<'_, '_>,
) -> Result<GraphPathResult, RetrievalError> {
    let ids = input
        .path
        .iter()
        .map(|(symbol_id, _)| symbol_id.clone())
        .collect::<Vec<_>>();
    let symbols = database
        .current_symbols_by_ids(CurrentSymbolSetLookup::new(
            input.request.project_id(),
            input.expected_generation_id,
            &ids,
        ))
        .await?;
    let mut symbols = symbols
        .into_iter()
        .map(|symbol| (symbol.symbol_id().clone(), symbol))
        .collect::<BTreeMap<_, _>>();
    let mut steps = Vec::with_capacity(input.path.len());
    for (symbol_id, arc) in input.path {
        let symbol =
            symbols
                .remove(&symbol_id)
                .ok_or(cartograph_db::StorageError::CorruptStoredValue {
                    field: "graph_path_symbol",
                })?;
        let via = arc.map(|arc| traversal_hop(TraversalDirection::Outgoing, arc));
        steps.push(GraphPathStep::new(symbol, via));
    }
    Ok(GraphPathResult::new(GraphPathResultInput {
        start: input.request.start().clone(),
        target: input.request.target().clone(),
        path: Some(steps),
        truncated: input.truncated,
    }))
}

async fn run_traversal(
    database: &CartographDatabase,
    execution: TraversalExecution<'_, '_>,
) -> Result<TraversalResult, RetrievalError> {
    let discovery = discover_graph(database, execution).await?;
    hydrate_traversal(
        database,
        TraversalHydrationInput {
            execution,
            discovery,
        },
    )
    .await
}

const fn traversal_plan(kind: TraversalKind, direction: TraversalDirection) -> TraversalPlan {
    let database_direction = match direction {
        TraversalDirection::Outgoing => GraphDirection::Outgoing,
        TraversalDirection::Incoming => GraphDirection::Incoming,
    };
    TraversalPlan {
        kind,
        direction,
        database_direction,
    }
}

async fn discover_graph(
    database: &CartographDatabase,
    execution: TraversalExecution<'_, '_>,
) -> Result<GraphDiscovery, RetrievalError> {
    let request = execution.request;
    let plan = execution.plan;
    let budget = request.budget();
    let mut visited = request.roots().iter().cloned().collect::<BTreeSet<_>>();
    let mut frontier = request.roots().to_vec();
    let mut discoveries = BTreeMap::new();
    let mut truncated = false;
    for depth in 1..=budget.max_depth() {
        let edges = database
            .current_graph_edges(
                CurrentGraphLookup::new(
                    CurrentGenerationLookup::new(
                        request.project_id(),
                        execution.expected_generation_id,
                    ),
                    &frontier,
                    plan.database_direction,
                )
                .with_limit(GRAPH_EDGE_READ_LIMIT)
                .with_test_targets(request.include_test_nodes()),
            )
            .await?;
        truncated |= edges.len() == usize::from(GRAPH_EDGE_READ_LIMIT);
        let arcs = edges
            .iter()
            .map(GraphArc::from_record)
            .filter(|arc| arc.confidence >= request.minimum_confidence())
            .filter(|arc| edge_is_relevant(arc, plan.kind, request.edge_kind()))
            .filter(|arc| !visited.contains(arc.adjacent(plan.direction)))
            .collect::<Vec<_>>();
        let remaining = usize::from(budget.max_nodes()).saturating_sub(discoveries.len());
        if remaining == 0 {
            truncated |= !arcs.is_empty();
            break;
        }
        let expansion = expand_frontier(FrontierInput {
            frontier: &frontier,
            arcs: &arcs,
            direction: plan.direction,
            max_new_nodes: remaining,
        });
        truncated |= expansion.truncated;
        record_discoveries(
            DiscoveryStep {
                depth,
                direction: plan.direction,
                expansion: &expansion,
            },
            &mut visited,
            &mut discoveries,
        );
        frontier = expansion.next;
        if frontier.is_empty() {
            break;
        }
    }
    Ok(GraphDiscovery {
        direction: plan.direction,
        discoveries,
        truncated,
    })
}

fn record_discoveries(
    step: DiscoveryStep<'_>,
    visited: &mut BTreeSet<SymbolId>,
    discoveries: &mut BTreeMap<SymbolId, (u8, GraphArc)>,
) {
    for symbol_id in &step.expansion.next {
        let best_arc = strongest_arc(&step.expansion.arcs, symbol_id, step.direction);
        if let Some(best_arc) = best_arc {
            visited.insert(symbol_id.clone());
            discoveries.insert(symbol_id.clone(), (step.depth, best_arc.clone()));
        }
    }
}

async fn hydrate_traversal(
    database: &CartographDatabase,
    mut input: TraversalHydrationInput<'_, '_>,
) -> Result<TraversalResult, RetrievalError> {
    let ids = input
        .discovery
        .discoveries
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let symbols = database
        .current_symbols_by_ids(CurrentSymbolSetLookup::new(
            input.execution.request.project_id(),
            input.execution.expected_generation_id,
            &ids,
        ))
        .await?;
    let mut symbols = symbols
        .into_iter()
        .map(|symbol| (symbol.symbol_id().clone(), symbol))
        .collect::<BTreeMap<_, _>>();
    let mut nodes = Vec::with_capacity(input.discovery.discoveries.len());
    for (symbol_id, (depth, arc)) in input.discovery.discoveries {
        let Some(symbol) = symbols.remove(&symbol_id) else {
            input.discovery.truncated = true;
            continue;
        };
        nodes.push(TraversalNode::new(
            symbol,
            depth,
            traversal_hop(input.discovery.direction, arc),
        ));
    }
    nodes.sort_by(traversal_node_order);
    Ok(TraversalResult {
        direction: input.discovery.direction,
        roots: input.execution.request.roots().to_vec(),
        nodes,
        truncated: input.discovery.truncated,
    })
}

fn traversal_hop(direction: TraversalDirection, arc: GraphArc) -> TraversalHop {
    TraversalHop {
        from_symbol_id: arc.origin(direction).clone(),
        to_symbol_id: arc.adjacent(direction).clone(),
        edge_source_symbol_id: arc.source,
        edge_target_symbol_id: arc.target,
        edge_kind: arc.edge_kind,
        confidence: arc.confidence,
        provenance: arc.provenance,
        site_count: arc.site_count,
    }
}

fn traversal_node_order(left: &TraversalNode, right: &TraversalNode) -> std::cmp::Ordering {
    left.depth()
        .cmp(&right.depth())
        .then_with(|| {
            left.symbol()
                .path()
                .as_str()
                .cmp(right.symbol().path().as_str())
        })
        .then_with(|| left.symbol().start_line().cmp(&right.symbol().start_line()))
        .then_with(|| left.symbol().symbol_id().cmp(right.symbol().symbol_id()))
}

async fn build_context_packet_with_retry(
    retriever: &DeterministicRetriever,
    request: &ContextRequest,
    precomputed_channels: Option<RetrievalChannels>,
) -> Result<ContextPacket, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        let channels = if attempt == 0 {
            precomputed_channels.clone()
        } else {
            None
        };
        match build_context_packet(retriever, request, channels).await {
            Err(error) if attempt == 0 && is_generation_changed(&error) => {}
            result => return result,
        }
    }
    Err(cartograph_db::StorageError::CurrentGenerationChanged.into())
}

async fn build_context_packet(
    retriever: &DeterministicRetriever,
    request: &ContextRequest,
    precomputed_channels: Option<RetrievalChannels>,
) -> Result<ContextPacket, RetrievalError> {
    let generation = retriever
        .database
        .current_generation_record(request.project_id())
        .await?;
    let Some(generation) = generation else {
        let retrieval = empty_context_retrieval(request)?;
        return Ok(assemble_packet(empty_context_assembly(request, retrieval)));
    };
    let mut state = ContextBuildState::default();
    let generation_input = ContextGenerationInput {
        request,
        expected_generation_id: generation.generation_id(),
    };
    collect_anchor_evidence(retriever, generation_input, &mut state).await?;
    let retrieval =
        fuse_context_retrieval(retriever, generation_input, precomputed_channels).await?;
    collect_retrieval_evidence(&retrieval, &mut state);
    let direct_tests = if context_selects_tests(request.intent()) {
        collect_direct_tests(retriever, generation_input, &state.evidence).await?
    } else {
        DirectTestResult::default()
    };
    let (roots, roots_were_truncated) = bound_context_roots(state.roots.into_values());
    let graph = expand_context_graph(
        retriever,
        ContextExpansionInput {
            request,
            roots,
            evidence: &mut state.evidence,
            direct_tests,
            expected_generation_id: generation.generation_id(),
        },
    )
    .await?;
    Ok(assemble_packet(PacketAssembly {
        task: request.query(),
        generation: Some(GenerationEvidence::new(
            generation.generation_id().clone(),
            generation.sequence(),
        )),
        intent: request.intent(),
        graph_direction: request.graph_direction(),
        freshness: request.freshness(),
        retrieval,
        evidence: state.evidence,
        affected_tests: graph.affected_tests,
        evidence_limit: request.budget().evidence_limit(),
        truncated: roots_were_truncated || graph.truncated,
    }))
}

fn empty_context_assembly(
    request: &ContextRequest,
    retrieval: crate::HybridSearchPacket,
) -> PacketAssembly<'_> {
    PacketAssembly {
        task: request.query(),
        generation: None,
        intent: request.intent(),
        graph_direction: request.graph_direction(),
        freshness: request.freshness(),
        retrieval,
        evidence: Vec::new(),
        affected_tests: Vec::new(),
        evidence_limit: request.budget().evidence_limit(),
        truncated: false,
    }
}

fn empty_context_retrieval(
    request: &ContextRequest,
) -> Result<crate::HybridSearchPacket, RetrievalError> {
    fuse_search(context_search_input(request)?)
}

/// Resolve the request's retrieval channels for the expected generation and
/// fuse them, failing closed when any policy channel holds a candidate from
/// another generation.
async fn fuse_context_retrieval(
    retriever: &DeterministicRetriever,
    input: ContextGenerationInput<'_, '_>,
    precomputed_channels: Option<RetrievalChannels>,
) -> Result<crate::HybridSearchPacket, RetrievalError> {
    let channels = resolve_context_channels(retriever, input, precomputed_channels).await?;
    let include_semantic = context_uses_semantic(input.request);
    validate_channel_generation(&channels, input.expected_generation_id, include_semantic)?;
    fuse_search(context_search_input(input.request)?.with_channels(channels))
}

/// Fusion bounds and intent preference of a context request, before channels
/// are attached.
fn context_search_input(request: &ContextRequest) -> Result<HybridSearchInput, RetrievalError> {
    Ok(HybridSearchInput::new(
        request.search_mode(),
        request.semantic_readiness(),
        request.budget().candidate_limit(),
    )?
    .with_preference(request.intent().retrieval_preference(request.query())))
}

async fn collect_anchor_evidence(
    retriever: &DeterministicRetriever,
    input: ContextGenerationInput<'_, '_>,
    state: &mut ContextBuildState,
) -> Result<(), RetrievalError> {
    let exact_limit = input.request.budget().exact_limit();
    for anchor in input.request.anchors() {
        match anchor {
            ContextAnchor::ExactName(name) => {
                let query = ExactTextQuery::new(name, exact_limit)?;
                for symbol in retriever
                    .exact_name_at(ExactGenerationQuery {
                        project_id: input.request.project_id(),
                        expected_generation_id: input.expected_generation_id,
                        query,
                    })
                    .await?
                {
                    state.roots.insert(symbol.symbol_id().clone());
                    state
                        .evidence
                        .push(evidence_from_symbol(&symbol, EvidenceReason::ExactName));
                }
            }
            ContextAnchor::ExactPath(path) => {
                let query = ExactPathQuery::new(path, exact_limit)?;
                if let Some(result) = retriever
                    .exact_path_at(ExactGenerationQuery {
                        project_id: input.request.project_id(),
                        expected_generation_id: input.expected_generation_id,
                        query,
                    })
                    .await?
                {
                    collect_exact_path_evidence(&result, state);
                }
            }
            ContextAnchor::ExactReference(name) => {
                let query = ExactTextQuery::new(name, exact_limit)?;
                for reference in retriever
                    .exact_reference_at(ExactGenerationQuery {
                        project_id: input.request.project_id(),
                        expected_generation_id: input.expected_generation_id,
                        query,
                    })
                    .await?
                {
                    collect_reference_evidence(&reference, state);
                }
            }
        }
    }
    Ok(())
}

fn collect_exact_path_evidence(result: &ExactPathResult, state: &mut ContextBuildState) {
    state.evidence.push(evidence_from_file(result.file()));
    for symbol in result.symbols() {
        state.roots.insert(symbol.symbol_id().clone());
        state
            .evidence
            .push(evidence_from_symbol(symbol, EvidenceReason::ExactPath));
    }
}

fn collect_reference_evidence(reference: &CurrentReferenceRecord, state: &mut ContextBuildState) {
    if !state.seen_reference_ids.insert(reference.reference_id()) {
        return;
    }
    if let Some(symbol_id) = reference.target_symbol_id() {
        state.roots.insert(symbol_id.clone());
    }
    if let Some(symbol_id) = reference.owner_symbol_id() {
        state.roots.insert(symbol_id.clone());
    }
    state.evidence.push(evidence_from_reference(reference));
}

async fn resolve_context_channels(
    retriever: &DeterministicRetriever,
    input: ContextGenerationInput<'_, '_>,
    precomputed: Option<RetrievalChannels>,
) -> Result<RetrievalChannels, RetrievalError> {
    if let Some(channels) = precomputed {
        return Ok(channels);
    }
    let query = LexicalQuery::for_code_search(
        input.request.query(),
        input.request.budget().candidate_limit(),
    )?;
    let lexical = retriever
        .lexical_channel_for_generation(GenerationLexicalRequest::new(
            input.request.project_id().clone(),
            input.expected_generation_id.clone(),
            query,
        ))
        .await?;
    RetrievalChannels::new().with_channel(lexical)
}

fn collect_retrieval_evidence(
    retrieval: &crate::HybridSearchPacket,
    state: &mut ContextBuildState,
) {
    for item in retrieval.items() {
        if let Some(symbol_id) = item.document().symbol_id() {
            state.roots.insert(symbol_id.clone());
        }
        state.evidence.push(evidence_from_fused_item(item));
    }
}

fn validate_channel_generation(
    channels: &RetrievalChannels,
    current_generation: &cartograph_domain::GenerationId,
    include_semantic: bool,
) -> Result<(), RetrievalError> {
    let mismatched = policy_channels(channels, include_semantic)
        .into_iter()
        .flatten()
        .flat_map(ChannelResults::candidates)
        .any(|candidate| candidate.document().generation_id() != current_generation);
    if mismatched {
        return Err(cartograph_db::StorageError::CurrentGenerationChanged.into());
    }
    Ok(())
}

fn context_uses_semantic(request: &ContextRequest) -> bool {
    request.search_mode() != SearchMode::Deterministic
        && request.semantic_readiness() == SemanticReadiness::Ready
}

fn policy_channels(
    channels: &RetrievalChannels,
    include_semantic: bool,
) -> [Option<&ChannelResults>; 2] {
    [
        channels.lexical(),
        include_semantic.then_some(channels.semantic()).flatten(),
    ]
}

fn bound_context_roots(mut roots: Vec<SymbolId>) -> (Vec<SymbolId>, bool) {
    let truncated = roots.len() > MAX_CONTEXT_ROOTS;
    roots.truncate(MAX_CONTEXT_ROOTS);
    (roots, truncated)
}

async fn collect_direct_tests(
    retriever: &DeterministicRetriever,
    input: ContextGenerationInput<'_, '_>,
    evidence: &[EvidenceItem],
) -> Result<DirectTestResult, RetrievalError> {
    let limit = input.request.budget().affected_test_limit();
    let (candidates, candidates_were_truncated) = direct_test_candidates(evidence, limit);
    let symbol_ids = candidates
        .iter()
        .filter_map(|candidate| match candidate {
            DirectTestCandidate::Symbol(symbol_id) => Some(symbol_id.clone()),
            DirectTestCandidate::File(_) => None,
        })
        .collect::<Vec<_>>();
    let symbols = retriever
        .database
        .current_symbols_by_ids(CurrentSymbolSetLookup::new(
            input.request.project_id(),
            input.expected_generation_id,
            &symbol_ids,
        ))
        .await?
        .into_iter()
        .map(|symbol| (symbol.symbol_id().clone(), symbol))
        .collect::<BTreeMap<_, _>>();
    let mut tests = Vec::new();
    let mut seen_files = BTreeSet::new();
    let mut truncated = candidates_were_truncated;
    for candidate in candidates {
        let symbol = match candidate {
            DirectTestCandidate::Symbol(symbol_id) => symbols.get(&symbol_id).cloned(),
            DirectTestCandidate::File(file_id) => retriever
                .database
                .current_symbols_by_file(CurrentFileSymbolsLookup::new(
                    CurrentGenerationLookup::new(
                        input.request.project_id(),
                        input.expected_generation_id,
                    ),
                    &file_id,
                    1,
                ))
                .await?
                .into_iter()
                .next(),
        };
        let Some(symbol) = symbol else {
            truncated = true;
            continue;
        };
        if seen_files.insert(symbol.file_id().clone()) {
            tests.push(AffectedTest::new(
                symbol,
                0,
                "direct-test-document".to_owned(),
            ));
        }
    }
    Ok(DirectTestResult { tests, truncated })
}

fn direct_test_candidates(
    evidence: &[EvidenceItem],
    limit: u16,
) -> (Vec<DirectTestCandidate>, bool) {
    let mut seen_files = BTreeSet::new();
    let mut seen_symbols = BTreeSet::new();
    let mut candidates = Vec::new();
    let mut truncated = false;
    for item in evidence
        .iter()
        .filter(|item| item.document_kind() == Some(DocumentKind::Test))
    {
        let candidate = if let Some(file_id) = item.file_id() {
            if !seen_files.insert(file_id.clone()) {
                continue;
            }
            item.symbol_id().map_or_else(
                || DirectTestCandidate::File(file_id.clone()),
                |symbol_id| DirectTestCandidate::Symbol(symbol_id.clone()),
            )
        } else if let Some(symbol_id) = item.symbol_id() {
            if !seen_symbols.insert(symbol_id.clone()) {
                continue;
            }
            DirectTestCandidate::Symbol(symbol_id.clone())
        } else {
            continue;
        };
        if candidates.len() < usize::from(limit) {
            candidates.push(candidate);
        } else {
            truncated = true;
        }
    }
    (candidates, truncated)
}

async fn expand_context_graph(
    retriever: &DeterministicRetriever,
    input: ContextExpansionInput<'_, '_, '_>,
) -> Result<ContextGraphResult, RetrievalError> {
    let Some(kind) = context_traversal_kind(input.request.intent()) else {
        return Ok(ContextGraphResult {
            affected_tests: input.direct_tests.tests,
            truncated: input.direct_tests.truncated,
        });
    };
    let Some(graph_direction) = input.request.graph_direction() else {
        return Ok(ContextGraphResult {
            affected_tests: input.direct_tests.tests,
            truncated: input.direct_tests.truncated,
        });
    };
    if input.roots.is_empty() {
        return Ok(ContextGraphResult {
            affected_tests: input.direct_tests.tests,
            truncated: input.direct_tests.truncated,
        });
    }
    let budget = input.request.budget();
    let traversal_request = TraversalRequest::new(
        input.request.project_id().clone(),
        input.roots,
        budget.traversal(),
    )?;
    let traversals = context_traversals(
        retriever,
        ContextTraversalsInput {
            request: &traversal_request,
            expected_generation_id: input.expected_generation_id,
            kind,
            direction: graph_direction,
        },
    )
    .await?;
    let traversal_was_truncated = traversals.iter().any(TraversalResult::truncated);
    let (nodes, nodes_were_truncated) =
        bound_context_graph_nodes(&traversals, budget.traversal().max_nodes());
    input
        .evidence
        .extend(nodes.iter().map(evidence_from_traversal_node));
    let (affected_tests, tests_were_truncated) = if context_selects_tests(input.request.intent()) {
        let (graph_tests, graph_tests_were_truncated) =
            affected_tests_from_nodes(&nodes, budget.affected_test_limit());
        let (tests, union_was_truncated) = merge_affected_tests(
            input.direct_tests.tests,
            graph_tests,
            budget.affected_test_limit(),
        );
        (
            tests,
            input.direct_tests.truncated || graph_tests_were_truncated || union_was_truncated,
        )
    } else {
        (Vec::new(), false)
    };
    Ok(ContextGraphResult {
        affected_tests,
        truncated: traversal_was_truncated || nodes_were_truncated || tests_were_truncated,
    })
}

fn merge_affected_tests(
    direct: Vec<AffectedTest>,
    graph: Vec<AffectedTest>,
    limit: u16,
) -> (Vec<AffectedTest>, bool) {
    let mut seen_files = BTreeSet::new();
    let mut tests = Vec::new();
    let mut truncated = false;
    for test in direct.into_iter().chain(graph) {
        if !seen_files.insert(test.symbol().file_id().clone()) {
            continue;
        }
        if tests.len() < usize::from(limit) {
            tests.push(test);
        } else {
            truncated = true;
        }
    }
    (tests, truncated)
}

async fn context_traversals(
    retriever: &DeterministicRetriever,
    input: ContextTraversalsInput<'_, '_>,
) -> Result<Vec<TraversalResult>, RetrievalError> {
    let directions = context_directions(input.direction);
    let mut traversals = Vec::with_capacity(directions.len());
    for direction in directions.iter().copied() {
        traversals.push(
            run_traversal(
                &retriever.database,
                TraversalExecution {
                    request: input.request,
                    expected_generation_id: input.expected_generation_id,
                    plan: traversal_plan(input.kind, direction),
                },
            )
            .await?,
        );
    }
    Ok(traversals)
}

const fn context_directions(direction: ContextGraphDirection) -> &'static [TraversalDirection] {
    match direction {
        ContextGraphDirection::Callers => &CALLER_DIRECTIONS,
        ContextGraphDirection::Callees => &CALLEE_DIRECTIONS,
        ContextGraphDirection::Both => &BIDIRECTIONAL_DIRECTIONS,
    }
}

fn bound_context_graph_nodes(
    traversals: &[TraversalResult],
    limit: u16,
) -> (Vec<TraversalNode>, bool) {
    let mut positions = BTreeMap::<SymbolId, usize>::new();
    let mut nodes = Vec::<TraversalNode>::new();
    for node in traversals
        .iter()
        .flat_map(|traversal| traversal.nodes().iter())
    {
        let symbol_id = node.symbol().symbol_id();
        if let Some(position) = positions.get(symbol_id).copied() {
            if context_graph_node_order(node, &nodes[position]).is_lt() {
                nodes[position] = node.clone();
            }
        } else {
            positions.insert(symbol_id.clone(), nodes.len());
            nodes.push(node.clone());
        }
    }
    nodes.sort_by(context_graph_node_order);
    let truncated = nodes.len() > usize::from(limit);
    nodes.truncate(usize::from(limit));
    (nodes, truncated)
}

fn context_graph_node_order(left: &TraversalNode, right: &TraversalNode) -> std::cmp::Ordering {
    left.depth()
        .cmp(&right.depth())
        .then_with(|| right.via().confidence().total_cmp(&left.via().confidence()))
        .then_with(|| right.via().site_count().cmp(&left.via().site_count()))
        .then_with(|| {
            left.symbol()
                .path()
                .as_str()
                .cmp(right.symbol().path().as_str())
        })
        .then_with(|| left.symbol().start_line().cmp(&right.symbol().start_line()))
        .then_with(|| left.symbol().symbol_id().cmp(right.symbol().symbol_id()))
}

const fn context_traversal_kind(intent: TaskIntent) -> Option<TraversalKind> {
    match intent {
        TaskIntent::ImplementationTrace => Some(TraversalKind::Calls),
        TaskIntent::ArchitectureSurvey
        | TaskIntent::ChangePlanning
        | TaskIntent::TestSelection
        | TaskIntent::ErrorDiagnosis => Some(TraversalKind::Impact),
        TaskIntent::SymbolLookup | TaskIntent::DocumentationLookup => None,
    }
}

const fn context_selects_tests(intent: TaskIntent) -> bool {
    matches!(
        intent,
        TaskIntent::ChangePlanning | TaskIntent::TestSelection | TaskIntent::ErrorDiagnosis
    )
}

async fn build_review_packet_with_retry(
    retriever: &DeterministicRetriever,
    request: &ReviewRequest,
) -> Result<ReviewPacket, RetrievalError> {
    for attempt in 0..GENERATION_ATTEMPTS {
        match build_review_packet(retriever, request).await {
            Err(error) if attempt == 0 && is_generation_changed(&error) => {}
            result => return result,
        }
    }
    Err(cartograph_db::StorageError::CurrentGenerationChanged.into())
}

async fn build_review_packet(
    retriever: &DeterministicRetriever,
    request: &ReviewRequest,
) -> Result<ReviewPacket, RetrievalError> {
    let Some(project_id) = request.project_id() else {
        return Ok(assemble_review_packet(empty_review_assembly(request)));
    };
    let generation = retriever
        .database
        .current_generation_record(project_id)
        .await?;
    let Some(generation) = generation else {
        return Ok(assemble_review_packet(empty_review_assembly(request)));
    };
    let mut state = collect_changed_file_evidence(
        retriever,
        ReviewGenerationInput {
            project_id,
            expected_generation_id: generation.generation_id(),
            request,
        },
    )
    .await?;
    let roots = std::mem::take(&mut state.roots).into_iter().collect();
    let graph = expand_review_graph(
        retriever,
        ReviewExpansionInput {
            project_id,
            expected_generation_id: generation.generation_id(),
            request,
            roots,
            evidence: &mut state.evidence,
        },
    )
    .await?;
    Ok(assemble_review_packet(ReviewAssembly {
        generation: Some(GenerationEvidence::new(
            generation.generation_id().clone(),
            generation.sequence(),
        )),
        freshness: request.freshness(),
        changed_file_count: request.changed_paths().len(),
        indexed_changed_files: state.indexed_changed_files,
        evidence: state.evidence,
        affected_tests: graph.affected_tests,
        evidence_limit: request.budget().evidence_limit(),
        truncation: ReviewTruncation::default()
            .with_changed_files_truncation(request.changed_files_truncated())
            .with_symbol_roots_truncation(state.symbol_roots_truncated)
            .with_graph_truncation(graph.graph_truncated)
            .with_affected_tests_truncation(graph.affected_tests_truncated),
    }))
}

fn empty_review_assembly(request: &ReviewRequest) -> ReviewAssembly {
    ReviewAssembly {
        generation: None,
        freshness: request.freshness(),
        changed_file_count: request.changed_paths().len(),
        indexed_changed_files: Vec::new(),
        evidence: Vec::new(),
        affected_tests: Vec::new(),
        evidence_limit: request.budget().evidence_limit(),
        truncation: ReviewTruncation::default()
            .with_changed_files_truncation(request.changed_files_truncated()),
    }
}

async fn collect_changed_file_evidence(
    retriever: &DeterministicRetriever,
    input: ReviewGenerationInput<'_, '_>,
) -> Result<ReviewBuildState, RetrievalError> {
    let mut state = ReviewBuildState::default();
    let budget = input.request.budget();
    for path in input.request.changed_paths() {
        let query = ExactPathQuery::new(path, budget.symbols_per_file())?;
        let Some(result) = retriever
            .exact_path_at(ExactGenerationQuery {
                project_id: input.project_id,
                expected_generation_id: input.expected_generation_id,
                query,
            })
            .await?
        else {
            continue;
        };
        state.indexed_changed_files.push(path.clone());
        state.evidence.push(evidence_from_file(result.file()));
        if result.symbols().len() == usize::from(budget.symbols_per_file()) {
            state.symbol_roots_truncated = true;
        }
        collect_review_symbols(&result, budget.root_limit(), &mut state);
    }
    Ok(state)
}

fn collect_review_symbols(result: &ExactPathResult, root_limit: u16, state: &mut ReviewBuildState) {
    for symbol in result.symbols() {
        if state.roots.contains(symbol.symbol_id()) {
            continue;
        }
        if state.roots.len() >= usize::from(root_limit) {
            state.symbol_roots_truncated = true;
            continue;
        }
        state.roots.insert(symbol.symbol_id().clone());
        state
            .evidence
            .push(evidence_from_symbol(symbol, EvidenceReason::ExactPath));
    }
}

async fn expand_review_graph(
    retriever: &DeterministicRetriever,
    input: ReviewExpansionInput<'_, '_, '_>,
) -> Result<ReviewGraphResult, RetrievalError> {
    if input.roots.is_empty() {
        return Ok(ReviewGraphResult {
            affected_tests: Vec::new(),
            graph_truncated: false,
            affected_tests_truncated: false,
        });
    }
    let budget = input.request.budget();
    let traversal_request =
        TraversalRequest::new(input.project_id.clone(), input.roots, budget.traversal())?;
    let impact = run_traversal(
        &retriever.database,
        TraversalExecution {
            request: &traversal_request,
            expected_generation_id: input.expected_generation_id,
            plan: traversal_plan(TraversalKind::Impact, TraversalDirection::Incoming),
        },
    )
    .await?;
    input
        .evidence
        .extend(impact.nodes().iter().map(evidence_from_traversal_node));
    let (affected_tests, affected_tests_truncated) =
        affected_tests_from_traversal(&impact, budget.affected_test_limit());
    Ok(ReviewGraphResult {
        affected_tests,
        graph_truncated: impact.truncated(),
        affected_tests_truncated,
    })
}

fn edge_is_relevant(arc: &GraphArc, kind: TraversalKind, edge_kind: Option<EdgeKind>) -> bool {
    if let Some(edge_kind) = edge_kind {
        return arc.edge_kind == edge_kind.as_str();
    }
    match kind {
        TraversalKind::Calls => arc.edge_kind == "calls",
        TraversalKind::Impact => matches!(
            arc.edge_kind.as_str(),
            "calls"
                | "imports"
                | "references"
                | "implements"
                | "extends"
                | "tests"
                | "type_of"
                | "returns"
                | "instantiates"
                | "overrides"
                | "decorates"
                | "field_access"
                | "def_use"
                | "exports"
        ),
    }
}

#[cfg(test)]
mod tests {
    use cartograph_domain::{EdgeKind, FileId};

    use super::*;
    use crate::model::{SearchEvidenceFixture, enrich_search_evidence_fixture, evidence_fixture};
    use crate::traversal::GraphArcFixture;
    use crate::{ContextBudget, ContextRequestOptions, IndexFreshness, TraversalBudget};

    fn project_id() -> ProjectId {
        ProjectId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap_or_else(|error| panic!("project fixture failed: {error}"))
    }

    fn context_request(task: &str) -> ContextRequest {
        ContextRequest::new(
            project_id(),
            task,
            ContextRequestOptions::new(IndexFreshness::Current, ContextBudget::default()),
        )
        .unwrap_or_else(|error| panic!("context request failed: {error}"))
    }

    fn symbol_id(index: u32) -> SymbolId {
        let value = format!("{index:08x}-1111-4111-8111-111111111111");
        SymbolId::parse(&value).unwrap_or_else(|error| panic!("symbol fixture failed: {error}"))
    }

    #[test]
    fn incoming_and_outgoing_hops_preserve_the_same_stored_edge_direction() {
        let arc = GraphArc::fixture(GraphArcFixture {
            source: symbol_id(1),
            target: symbol_id(2),
            edge_kind: "calls",
            confidence: 1.0,
            site_count: 1,
        });
        let incoming = traversal_hop(TraversalDirection::Incoming, arc.clone());
        let outgoing = traversal_hop(TraversalDirection::Outgoing, arc);
        assert_eq!(incoming.from_symbol_id(), &symbol_id(2));
        assert_eq!(incoming.to_symbol_id(), &symbol_id(1));
        for hop in [incoming, outgoing] {
            let value =
                serde_json::to_value(hop).unwrap_or_else(|error| panic!("serialized hop: {error}"));
            assert_eq!(value["edge_source_symbol_id"], symbol_id(1).as_str());
            assert_eq!(value["edge_target_symbol_id"], symbol_id(2).as_str());
        }
    }

    #[test]
    fn intent_selects_graph_direction_and_test_expansion_explicitly() {
        assert!(matches!(
            context_traversal_kind(TaskIntent::ImplementationTrace),
            Some(TraversalKind::Calls)
        ));
        assert!(matches!(
            context_traversal_kind(TaskIntent::ChangePlanning),
            Some(TraversalKind::Impact)
        ));
        assert!(context_traversal_kind(TaskIntent::SymbolLookup).is_none());
        assert!(context_traversal_kind(TaskIntent::DocumentationLookup).is_none());
        assert!(context_selects_tests(TaskIntent::TestSelection));
        assert!(context_selects_tests(TaskIntent::ErrorDiagnosis));
        assert!(!context_selects_tests(TaskIntent::ArchitectureSurvey));
    }

    #[test]
    fn caller_callee_and_architecture_requests_select_only_declared_directions() {
        let incoming_graph_request = context_request("trace callers of publish_generation")
            .with_intent(TaskIntent::ImplementationTrace);
        assert_eq!(
            incoming_graph_request.graph_direction(),
            Some(ContextGraphDirection::Callers)
        );
        assert_eq!(
            context_directions(ContextGraphDirection::Callers),
            &[TraversalDirection::Incoming]
        );

        let outgoing_graph_request = context_request("trace what publish_generation calls");
        assert_eq!(
            outgoing_graph_request.graph_direction(),
            Some(ContextGraphDirection::Callees)
        );
        assert_eq!(
            context_directions(ContextGraphDirection::Callees),
            &[TraversalDirection::Outgoing]
        );

        let architecture = context_request("survey storage architecture");
        assert_eq!(
            architecture.graph_direction(),
            Some(ContextGraphDirection::Both)
        );
        assert_eq!(
            context_directions(ContextGraphDirection::Both),
            &[TraversalDirection::Incoming, TraversalDirection::Outgoing]
        );
    }

    #[test]
    fn context_root_bound_preserves_anchor_and_fused_admission_order() {
        let strongest_anchor = symbol_id(u32::MAX);
        let mut roots = OrderedRoots::default();
        roots.insert(strongest_anchor.clone());
        let root_count = u32::try_from(MAX_CONTEXT_ROOTS)
            .unwrap_or_else(|error| panic!("context-root bound does not fit u32: {error}"));
        for index in 0..root_count {
            roots.insert(symbol_id(index));
        }
        let (bounded, truncated) = bound_context_roots(roots.into_values());
        assert!(truncated);
        assert_eq!(bounded.len(), MAX_CONTEXT_ROOTS);
        assert_eq!(bounded[0], strongest_anchor);
        assert_eq!(bounded[1], symbol_id(0));
        assert_eq!(bounded[MAX_CONTEXT_ROOTS - 1], symbol_id(30));
    }

    #[test]
    fn direct_test_file_documents_survive_without_a_symbol_root() {
        let file_id = FileId::parse("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .unwrap_or_else(|error| panic!("file fixture failed: {error}"));
        let evidence = enrich_search_evidence_fixture(
            evidence_fixture("tests/search.rs", "", EvidenceReason::Bm25),
            SearchEvidenceFixture {
                file_id: Some(file_id.clone()),
                symbol_id: None,
                language: SourceLanguage::Rust,
                document_kind: DocumentKind::Test,
                fused_rank: Some(1),
                reciprocal_rank_score: Some(1.0),
            },
        );
        let (candidates, truncated) = direct_test_candidates(&[evidence], 1);
        assert_eq!(candidates, vec![DirectTestCandidate::File(file_id)]);
        assert!(!truncated);
    }

    #[test]
    fn explicit_graph_edge_filter_is_exact_and_overrides_direction_defaults() {
        let imports = GraphArc::fixture(GraphArcFixture {
            source: symbol_id(1),
            target: symbol_id(2),
            edge_kind: "imports",
            confidence: 1.0,
            site_count: 1,
        });
        assert!(!edge_is_relevant(&imports, TraversalKind::Calls, None));
        assert!(edge_is_relevant(
            &imports,
            TraversalKind::Calls,
            Some(EdgeKind::Imports)
        ));
        assert!(!edge_is_relevant(
            &imports,
            TraversalKind::Impact,
            Some(EdgeKind::Calls)
        ));
    }

    #[test]
    fn path_reconstruction_retains_the_exact_ordered_parent_chain() {
        let start = symbol_id(1);
        let middle = symbol_id(2);
        let target = symbol_id(3);
        let request = GraphPathRequest::new(crate::GraphPathRequestInput {
            project_id: project_id(),
            start: start.clone(),
            target: target.clone(),
            budget: TraversalBudget::new(4, 20)
                .unwrap_or_else(|error| panic!("path budget failed: {error}")),
        });
        let first = GraphArc::fixture(GraphArcFixture {
            source: start.clone(),
            target: middle.clone(),
            edge_kind: "calls",
            confidence: 0.9,
            site_count: 1,
        });
        let second = GraphArc::fixture(GraphArcFixture {
            source: middle.clone(),
            target: target.clone(),
            edge_kind: "returns",
            confidence: 0.8,
            site_count: 2,
        });
        let parents = BTreeMap::from([(middle.clone(), first), (target.clone(), second)]);
        let path = reconstruct_graph_path(&request, &parents)
            .unwrap_or_else(|error| panic!("path reconstruction failed: {error}"));
        let ids = path
            .iter()
            .map(|(symbol_id, _)| symbol_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![start, middle, target]);
        assert!(path[0].1.is_none());
        assert_eq!(
            path[1].1.as_ref().map(|arc| arc.edge_kind.as_str()),
            Some("calls")
        );
        assert_eq!(
            path[2].1.as_ref().map(|arc| arc.edge_kind.as_str()),
            Some("returns")
        );
    }
}
