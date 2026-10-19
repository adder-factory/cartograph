//! Current-source neighbor selection uses the same binary vectors as top-K.

use super::{
    NAMESPACE,
    vectors::{active, cosine_distance, current, documents_scan, hit, read_vector, source},
};
use crate::semantic::types::{
    EmbeddingModelSelector, EmbeddingModelState, RegisteredEmbeddingModel,
    SemanticReadinessState as Ready, SemanticStorageError as Error, SimilarSymbolHit,
    SimilarSymbolsRequest, SimilarSymbolsResult, SimilarSymbolsResultInput,
};
use crate::{
    CanonicalSearchDocument, NativeDatabase,
    native_database::visit_state,
    retrieval::native::retain,
    rocks::{ReadBudget, StateTransaction},
};
use cartograph_domain::{DocumentId, DocumentKind};
use std::ops::ControlFlow;

struct Source {
    model: EmbeddingModelSelector,
    document: DocumentId,
    language: String,
    vector: Vec<f32>,
}

impl NativeDatabase {
    /// Compare the source's exact current stored vector with current symbol documents.
    /// # Errors
    /// Rejects absent/ambiguous source models, stale source, or incomplete model coverage.
    pub async fn similar_current_symbols(
        &self,
        request: SimilarSymbolsRequest,
    ) -> Result<SimilarSymbolsResult, Error> {
        self.require_project(&request.project_id)?;
        self.transaction(
            Some(request.expected_generation_id.clone()),
            0,
            0,
            request.statement_timeout,
            move |transaction, budget| {
                let generation = current(transaction)?;
                let source = resolve_source(transaction, &request, budget)?;
                active(transaction, &source.model, budget)?;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let mut candidates = Vec::new();
                let (mut documents, mut embedded) = (0_u64, 0_u64);
                documents_scan(&graph, budget, |_, document, budget| {
                    documents += 1;
                    let id = document.document_id.clone();
                    let eligible = eligible_neighbor(&document, &source, &request);
                    let (_, digest) =
                        source_text(transaction, &graph, &generation, document, budget)?;
                    if let Some(vector) =
                        read_vector(transaction, &source.model, &id, &digest, budget)?
                    {
                        embedded += 1;
                        if eligible {
                            let distance = cosine_distance(&source.vector, &vector, budget)?;
                            if 1.0 - distance >= request.minimum_score {
                                retain(
                                    &mut candidates,
                                    (id, distance),
                                    usize::from(request.limit) + 1,
                                    |a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)),
                                );
                            }
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
                if documents != embedded {
                    return Err(Error::NotReady {
                        state: Ready::CoverageIncomplete,
                    });
                }
                let truncated = candidates.len() > usize::from(request.limit);
                candidates.truncate(usize::from(request.limit));
                let hits = candidates
                    .into_iter()
                    .map(|(id, distance)| {
                        hit(&graph, &generation, &id, distance, budget)
                            .map(|hit| SimilarSymbolHit::new(hit, (1.0 - distance).clamp(0.0, 1.0)))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SimilarSymbolsResult::new(SimilarSymbolsResultInput {
                    model: source.model,
                    source_symbol_id: request.source_symbol_id,
                    hits,
                    truncated,
                }))
            },
        )
        .await
    }
}

fn eligible_neighbor(
    document: &CanonicalSearchDocument,
    source: &Source,
    request: &SimilarSymbolsRequest,
) -> bool {
    document.kind == DocumentKind::Symbol
        && document.symbol_id.is_some()
        && document.document_id != source.document
        && (!request.same_language || document.language == source.language)
}

fn source_models(
    transaction: &StateTransaction<'_>,
    request: &SimilarSymbolsRequest,
    budget: &mut ReadBudget,
) -> Result<Vec<RegisteredEmbeddingModel>, Error> {
    let mut models = Vec::new();
    visit_state(
        transaction,
        NAMESPACE,
        b"m/",
        budget,
        |_, model: RegisteredEmbeddingModel, _| {
            if model.state == EmbeddingModelState::Active
                && request
                    .model_id
                    .as_ref()
                    .is_none_or(|id| id == &model.selector.model_id)
            {
                models.push(model);
            }
            if models.len() > 256 {
                return Err(crate::rocks::RocksError::Limit {
                    resource: "embedding_models",
                });
            }
            Ok(())
        },
    )?;
    Ok(models)
}

fn resolve_source(
    transaction: &StateTransaction<'_>,
    request: &SimilarSymbolsRequest,
    budget: &mut ReadBudget,
) -> Result<Source, Error> {
    let models = source_models(transaction, request, budget)?;
    let snapshot = transaction.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let mut found = Vec::new();
    documents_scan(&graph, budget, |_, document, budget| {
        if document.kind != DocumentKind::Symbol
            || document.symbol_id.as_ref() != Some(&request.source_symbol_id)
        {
            return Ok(ControlFlow::Continue(()));
        }
        let (id, language) = (document.document_id.clone(), document.language.clone());
        let (_, digest) = source(
            transaction,
            &graph,
            &request.expected_generation_id,
            document,
            budget,
        )?;
        for model in &models {
            if let Some(vector) = read_vector(transaction, &model.selector, &id, &digest, budget)? {
                found.push(Source {
                    model: model.selector.clone(),
                    document: id.clone(),
                    language: language.clone(),
                    vector,
                });
                if found.len() > 1 {
                    return Err(Error::AmbiguousActiveModels);
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    found.pop().ok_or(Error::SourceEmbeddingUnavailable)
}

use super::vectors::source as source_text;
