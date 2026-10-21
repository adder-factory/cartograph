//! Binary vectors are reused across generations only after exact text revalidation.

use super::{NAMESPACE, load_model};
use crate::semantic::{
    common::{DocumentSource, validate_normalization},
    types::{
        EmbeddingBatchUpsertReport, EmbeddingBatchUpsertRequest, EmbeddingIndexStatus,
        EmbeddingModelSelector, EmbeddingModelState, EmbeddingPageCursor, PendingEmbeddingDocument,
        PendingEmbeddingPage, PendingEmbeddingPageRequest, RegisteredEmbeddingModel,
        RetireEmbeddingModelRequest, SemanticReadinessReport, SemanticReadinessRequest,
        SemanticReadinessState as Ready, SemanticStorageError as Error, VectorSearchHit,
        VectorSearchRequest,
    },
};
use crate::{
    CanonicalSearchDocument, NativeDatabase,
    artifacts::native::{bump_epoch, summaries::embedding_summary},
    rocks::{GraphSnapshot, ReadBudget, StateTransaction},
};
use cartograph_domain::{ContentDigest, DocumentId, GenerationId, SymbolKind};
use std::{ops::ControlFlow, time::Duration};

impl NativeDatabase {
    /// Inspect the native exact-cosine reader for an exact registered model.
    /// # Errors
    /// Rejects missing/mismatched models or exhausted native read bounds.
    pub async fn embedding_model_index_status(
        &self,
        selector: &EmbeddingModelSelector,
        timeout: Duration,
    ) -> Result<EmbeddingIndexStatus, Error> {
        let model = self.registered_embedding_model(selector, timeout).await?;
        Ok(EmbeddingIndexStatus {
            model_id: selector.model_id.clone(),
            index_name: format!("native-exact-cosine-v1/{}", selector.model_id),
            algorithm: "exact-cosine",
            ready: model.state == EmbeddingModelState::Active,
        })
    }

    /// Validate the native vector reader; it requires no external index creation.
    /// # Errors
    /// Rejects missing/mismatched models or exhausted native read bounds.
    pub async fn ensure_embedding_model_index(
        &self,
        selector: &EmbeddingModelSelector,
        timeout: Duration,
    ) -> Result<EmbeddingIndexStatus, Error> {
        self.embedding_model_index_status(selector, timeout).await
    }

    /// Page exact current inputs lacking a digest-compatible native vector.
    /// # Errors
    /// Rejects changed cursors, invalid models, oversized text, or native read failure.
    pub async fn pending_current_embedding_documents(
        &self,
        request: PendingEmbeddingPageRequest,
    ) -> Result<PendingEmbeddingPage, Error> {
        self.require_project(&request.project_id)?;
        self.transaction(
            None,
            0,
            0,
            request.statement_timeout,
            move |transaction, budget| {
                active(transaction, &request.model, budget)?;
                let generation = current(transaction)?;
                if request
                    .cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.generation_id != generation)
                {
                    return Err(Error::CurrentGenerationChanged);
                }
                let after = request.cursor.map_or(0, |cursor| cursor.after_row_id);
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let mut documents = Vec::new();
                let mut bytes = 0_u64;
                let mut more = false;
                documents_scan(&graph, budget, |ordinal, document, budget| {
                    if ordinal <= after {
                        return Ok(ControlFlow::Continue(()));
                    }
                    let id = document.document_id.clone();
                    let path = document.path.clone();
                    let (text, digest) =
                        source(transaction, &graph, &generation, document, budget)?;
                    if read_vector(transaction, &request.model, &id, &digest, budget)?.is_some() {
                        return Ok(ControlFlow::Continue(()));
                    }
                    let next = bytes.checked_add(text.len() as u64).ok_or(corrupt())?;
                    if !embedding_page_has_room(
                        documents.len(),
                        next,
                        (
                            usize::from(request.maximum_documents),
                            request.maximum_source_bytes,
                        ),
                    )? {
                        more = true;
                        return Ok(ControlFlow::Break(()));
                    }
                    bytes = next;
                    documents.push(PendingEmbeddingDocument {
                        row_id: ordinal,
                        document_id: id,
                        path,
                        text,
                        source_digest: digest,
                    });
                    Ok(ControlFlow::Continue(()))
                })?;
                let next_cursor = if more {
                    documents.last().map(|document| EmbeddingPageCursor {
                        generation_id: generation.clone(),
                        after_row_id: document.row_id,
                    })
                } else {
                    None
                };
                Ok(PendingEmbeddingPage {
                    generation_id: generation,
                    documents,
                    next_cursor,
                })
            },
        )
        .await
    }

    /// Atomically validate and write a bounded batch under one publication fence.
    /// # Errors
    /// Rejects changed source/model/generation, missing documents, or invalid vectors.
    pub async fn upsert_current_document_embeddings(
        &self,
        request: EmbeddingBatchUpsertRequest,
    ) -> Result<EmbeddingBatchUpsertReport, Error> {
        self.require_project(&request.project_id)?;
        self.transaction(
            Some(request.generation_id.clone()),
            2 * 1_048_576,
            129,
            request.statement_timeout,
            move |transaction, budget| {
                let model = active(transaction, &request.model, budget)?;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let mut writes = Vec::new();
                let mut source_bytes = 0_usize;
                for row in &request.rows {
                    validate_normalization(&row.vector, model.normalization)?;
                    let document = graph
                        .document(&row.document_id, budget)?
                        .ok_or(Error::DocumentNotFound)?;
                    let (text, digest) = source(
                        transaction,
                        &graph,
                        &request.generation_id,
                        document,
                        budget,
                    )?;
                    source_bytes = source_bytes.saturating_add(text.len());
                    if source_bytes > 16 * 1_048_576 {
                        return Err(Error::DocumentTooLarge);
                    }
                    if digest != row.source_digest {
                        return Err(Error::SourceDigestChanged);
                    }
                    let key = vector_key(&request.model, &row.document_id);
                    let value = encode_vector(&digest, &row.vector)?;
                    if transaction.get(NAMESPACE, &key, budget)?.as_deref()
                        != Some(value.as_slice())
                    {
                        writes.push((key, value));
                    }
                }
                drop(graph);
                drop(snapshot);
                let requested = u16::try_from(request.rows.len()).map_err(|_| corrupt())?;
                let written = u16::try_from(writes.len()).map_err(|_| corrupt())?;
                for (key, value) in writes {
                    transaction.put(NAMESPACE, &key, value)?;
                }
                if written > 0 {
                    bump_epoch(transaction, budget, b"vector_epoch")?;
                }
                Ok(EmbeddingBatchUpsertReport { requested, written })
            },
        )
        .await
    }

    /// Prove exact model identity, complete current text coverage, and a real cosine probe.
    /// # Errors
    /// Rejects project mismatch, corrupt vectors, or exhausted native read bounds.
    pub async fn semantic_readiness(
        &self,
        request: SemanticReadinessRequest,
    ) -> Result<SemanticReadinessReport, Error> {
        self.require_project(&request.project_id)?;
        let cache = self.semantic_cache.clone();
        self.transaction(
            None,
            0,
            0,
            request.statement_timeout,
            move |transaction, budget| {
                super::cache::readiness(&cache, transaction, &request.model, budget)
            },
        )
        .await
    }

    /// Execute bounded exact cosine top-K against one complete current generation.
    /// # Errors
    /// Rejects incomplete vector coverage, stale generations, or invalid native state.
    pub async fn vector_top_k(
        &self,
        request: VectorSearchRequest,
    ) -> Result<Vec<VectorSearchHit>, Error> {
        self.require_project(&request.project_id)?;
        let cache = self.semantic_cache.clone();
        self.transaction(
            Some(request.expected_generation_id.clone()),
            0,
            0,
            request.statement_timeout,
            move |transaction, budget| {
                let model = active(transaction, &request.model, budget)?;
                validate_normalization(&request.vector, model.normalization)?;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                super::cache::top_k(
                    &cache,
                    super::cache::VectorQuery {
                        transaction,
                        graph: &graph,
                        generation: &request.expected_generation_id,
                        model: &request.model,
                        vector: &request.vector,
                        limit: usize::from(request.limit),
                    },
                    budget,
                )
            },
        )
        .await
    }

    /// Retire a model only after its replacement proves complete current coverage.
    /// # Errors
    /// Rejects an unready replacement, mismatched identities, or native write failure.
    pub async fn retire_embedding_model(
        &self,
        request: RetireEmbeddingModelRequest,
    ) -> Result<RegisteredEmbeddingModel, Error> {
        self.transaction(
            None,
            1_048_576,
            2,
            request.statement_timeout,
            move |transaction, budget| {
                let mut retiring = load_model(transaction, &request.retiring, budget)?;
                if !readiness(transaction, &request.replacement, budget, |_, _, _| Ok(()))?.ready()
                {
                    return Err(Error::ReplacementNotReady);
                }
                retiring.state = EmbeddingModelState::Retired;
                crate::artifacts::native::bump_epoch(transaction, budget, b"vector_epoch")?;
                transaction.put(
                    NAMESPACE,
                    &super::model_key(&request.retiring),
                    crate::native_database::encode(&retiring)?,
                )?;
                Ok(retiring)
            },
        )
        .await
    }
}

fn embedding_page_has_room(
    documents: usize,
    next_bytes: u64,
    maximum: (usize, u64),
) -> Result<bool, Error> {
    if documents < maximum.0 && next_bytes <= maximum.1 {
        return Ok(true);
    }
    if documents == 0 {
        return Err(Error::DocumentTooLarge);
    }
    Ok(false)
}

pub(super) fn active(
    transaction: &StateTransaction<'_>,
    model: &EmbeddingModelSelector,
    budget: &mut ReadBudget,
) -> Result<RegisteredEmbeddingModel, Error> {
    let model = load_model(transaction, model, budget)?;
    if model.state == EmbeddingModelState::Retired {
        return Err(Error::ModelRetired);
    }
    Ok(model)
}
pub(super) fn current(transaction: &StateTransaction<'_>) -> Result<GenerationId, Error> {
    transaction
        .current()?
        .map(|revision| revision.generation_id)
        .ok_or(Error::CurrentGenerationUnavailable)
}

pub(crate) fn source(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    generation: &GenerationId,
    document: CanonicalSearchDocument,
    budget: &mut ReadBudget,
) -> Result<(String, ContentDigest), Error> {
    let summary = embedding_summary(transaction, graph, generation, &document, budget)?;
    DocumentSource {
        path: document.path,
        language: document.language,
        kind: document.kind,
        qualified_name: document.qualified_name,
        code: document.code,
        natural_text: document.natural_text,
        summary,
    }
    .render()
}

fn vector_key(model: &EmbeddingModelSelector, document: &DocumentId) -> Vec<u8> {
    format!("v/{}/{document}", model.model_id).into_bytes()
}
fn encode_vector(digest: &ContentDigest, vector: &[f32]) -> Result<Vec<u8>, Error> {
    let mut output = Vec::with_capacity(66 + vector.len() * 4);
    output.extend_from_slice(digest.as_str().as_bytes());
    output.extend_from_slice(
        &u16::try_from(vector.len())
            .map_err(|_| corrupt())?
            .to_le_bytes(),
    );
    for value in vector {
        output.extend_from_slice(&value.to_le_bytes());
    }
    Ok(output)
}
pub(super) fn read_vector(
    transaction: &StateTransaction<'_>,
    model: &EmbeddingModelSelector,
    document: &DocumentId,
    digest: &ContentDigest,
    budget: &mut ReadBudget,
) -> Result<Option<Vec<f32>>, Error> {
    let Some(bytes) = transaction.get(NAMESPACE, &vector_key(model, document), budget)? else {
        return Ok(None);
    };
    let (stored_digest, vector) = decode_vector(&bytes, model.dimension)?;
    Ok((stored_digest == *digest).then_some(vector))
}
pub(crate) fn decode_vector(
    bytes: &[u8],
    dimension: u16,
) -> Result<(ContentDigest, Vec<f32>), Error> {
    if bytes.len() != 66 + usize::from(dimension) * 4 {
        return Err(corrupt());
    }
    let digest = ContentDigest::parse(std::str::from_utf8(&bytes[..64]).map_err(|_| corrupt())?)
        .map_err(|_| corrupt())?;
    let stored = u16::from_le_bytes(bytes[64..66].try_into().map_err(|_| corrupt())?);
    if stored != dimension {
        return Err(corrupt());
    }
    let mut vector = Vec::with_capacity(usize::from(dimension));
    let mut magnitude = 0_f64;
    for bytes in bytes[66..].as_chunks::<4>().0 {
        let value = f32::from_le_bytes(*bytes);
        if !value.is_finite() {
            return Err(corrupt());
        }
        magnitude += f64::from(value).powi(2);
        vector.push(value);
    }
    if magnitude == 0.0 || !magnitude.is_finite() {
        return Err(corrupt());
    }
    Ok((digest, vector))
}

pub(super) fn readiness(
    transaction: &StateTransaction<'_>,
    selector: &EmbeddingModelSelector,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(u64, DocumentId, Vec<f32>) -> Result<(), Error>,
) -> Result<SemanticReadinessReport, Error> {
    let mut report = SemanticReadinessReport {
        model_id: selector.model_id.clone(),
        generation_id: transaction
            .current()?
            .map(|revision| revision.generation_id),
        documents: 0,
        embedded: 0,
        index_ready: false,
        query_probe_ready: false,
        state: Ready::ModelMissing,
    };
    let model = match load_model(transaction, selector, budget) {
        Ok(model) => model,
        Err(Error::ModelNotFound) => return Ok(report),
        Err(Error::ModelMismatch) => {
            report.state = Ready::ModelMismatch;
            return Ok(report);
        }
        Err(error) => return Err(error),
    };
    if model.state == EmbeddingModelState::Retired {
        report.state = Ready::ModelRetired;
        return Ok(report);
    }
    report.index_ready = true;
    let Some(generation) = &report.generation_id else {
        report.state = Ready::NoCurrentGeneration;
        return Ok(report);
    };
    let snapshot = transaction.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let expected_documents = graph.info().documents;
    let (documents, embedded) = visit_vectors(
        transaction,
        &graph,
        generation,
        selector,
        budget,
        |id, vector, budget| {
            validate_normalization(&vector, model.normalization)?;
            if !report.query_probe_ready {
                report.query_probe_ready = cosine_distance(&vector, &vector, budget)?.abs() <= 1e-9;
            }
            visit(expected_documents, id, vector)
        },
    )?;
    report.documents = documents;
    report.embedded = embedded;
    report.state = if report.documents == 0 {
        Ready::NoDocuments
    } else if report.documents != report.embedded {
        Ready::CoverageIncomplete
    } else if !report.query_probe_ready {
        Ready::QueryProbeFailed
    } else {
        Ready::Ready
    };
    Ok(report)
}

pub(super) fn visit_vectors(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    generation: &GenerationId,
    model: &EmbeddingModelSelector,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(DocumentId, Vec<f32>, &ReadBudget) -> Result<(), Error>,
) -> Result<(u64, u64), Error> {
    let (mut documents, mut embedded) = (0_u64, 0_u64);
    let mut cursor = None;
    loop {
        let (rows, next) = graph.documents(cursor.as_ref(), 64, budget)?;
        let mut inputs = Vec::with_capacity(rows.len());
        for document in rows {
            documents += 1;
            let id = document.document_id.clone();
            let (_, digest) = source(transaction, graph, generation, document, budget)?;
            inputs.push((id, digest));
        }
        embedded += visit_vector_batch(transaction, model, inputs, budget, &mut visit)?;
        cursor = next;
        if cursor.is_none() {
            return Ok((documents, embedded));
        }
    }
}

fn visit_vector_batch(
    transaction: &StateTransaction<'_>,
    model: &EmbeddingModelSelector,
    inputs: Vec<(DocumentId, ContentDigest)>,
    budget: &mut ReadBudget,
    visit: &mut impl FnMut(DocumentId, Vec<f32>, &ReadBudget) -> Result<(), Error>,
) -> Result<u64, Error> {
    if inputs.is_empty() {
        return Ok(0);
    }
    let keys = inputs
        .iter()
        .map(|(id, _)| vector_key(model, id))
        .collect::<Vec<_>>();
    let values = transaction.get_many(NAMESPACE, &keys, budget)?;
    let mut embedded = 0;
    for ((id, digest), value) in inputs.into_iter().zip(values) {
        budget.checkpoint()?;
        if let Some(value) = value {
            let (stored, vector) = decode_vector(&value, model.dimension)?;
            if stored == digest {
                embedded += 1;
                visit(id, vector, budget)?;
            }
        }
    }
    Ok(embedded)
}

pub(super) fn documents_scan(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(
        u64,
        CanonicalSearchDocument,
        &mut ReadBudget,
    ) -> Result<ControlFlow<()>, Error>,
) -> Result<(), Error> {
    let mut cursor = None;
    let mut ordinal = 0_u64;
    loop {
        let (documents, next) = graph.documents(cursor.as_ref(), 64, budget)?;
        for document in documents {
            budget.checkpoint()?;
            ordinal = ordinal.checked_add(1).ok_or(corrupt())?;
            if visit(ordinal, document, budget)?.is_break() {
                return Ok(());
            }
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

pub(super) fn top_k(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    generation: &GenerationId,
    model: &EmbeddingModelSelector,
    vector: &[f32],
    limit: usize,
    budget: &mut ReadBudget,
) -> Result<Vec<VectorSearchHit>, Error> {
    let mut selected = Vec::new();
    let (documents, embedded) = visit_vectors(
        transaction,
        graph,
        generation,
        model,
        budget,
        |id, candidate, budget| {
            let distance = cosine_distance(vector, &candidate, budget)?;
            crate::retrieval::native::retain(&mut selected, (id, distance), limit, |a, b| {
                a.1.total_cmp(&b.1).then(a.0.cmp(&b.0))
            });
            Ok(())
        },
    )?;
    if documents == 0 {
        return Err(Error::NotReady {
            state: Ready::NoDocuments,
        });
    }
    if documents != embedded {
        return Err(Error::NotReady {
            state: Ready::CoverageIncomplete,
        });
    }
    selected
        .into_iter()
        .map(|(id, distance)| hit(graph, generation, &id, distance, budget))
        .collect()
}

pub(super) fn cosine_distance(
    left: &[f32],
    right: &[f32],
    budget: &ReadBudget,
) -> Result<f64, Error> {
    if left.len() != right.len() {
        return Err(corrupt());
    }
    let (mut dot, mut a, mut b) = (0_f64, 0_f64, 0_f64);
    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        if index % 128 == 0 {
            budget.checkpoint()?;
        }
        let (left, right) = (f64::from(*left), f64::from(*right));
        dot += left * right;
        a += left * left;
        b += right * right;
    }
    let distance = 1.0 - dot / (a.sqrt() * b.sqrt());
    if !distance.is_finite() || !(-1e-9..=2.0 + 1e-9).contains(&distance) {
        return Err(corrupt());
    }
    Ok(distance.clamp(0.0, 2.0))
}

pub(super) fn hit(
    graph: &GraphSnapshot<'_, '_>,
    generation: &GenerationId,
    id: &DocumentId,
    distance: f64,
    budget: &mut ReadBudget,
) -> Result<VectorSearchHit, Error> {
    let document = graph.document(id, budget)?.ok_or(corrupt())?;
    let symbol_kind = document
        .symbol_id
        .as_ref()
        .map(|id| graph.symbol(id, budget))
        .transpose()?
        .flatten()
        .and_then(|symbol| SymbolKind::from_stable_str(&symbol.symbol_kind));
    let rerank = crate::semantic::rerank::render_rerank_text(&document, "")?;
    Ok(VectorSearchHit {
        generation_id: generation.clone(),
        document_id: document.document_id,
        file_id: document.file_id,
        symbol_id: document.symbol_id,
        path: document.path,
        language: document.language,
        document_kind: document.kind,
        qualified_name: document.qualified_name,
        symbol_kind,
        distance,
        rerank_text: Some(rerank),
    })
}

pub(super) fn corrupt() -> Error {
    Error::CorruptStoredValue {
        field: "native-vector",
    }
}
