//! One bounded corpus per owner, fenced by generation, model, vectors and summaries.
//! No expiry timer or filesystem event can confer semantic readiness.

use std::sync::{Arc, Mutex};

use cartograph_domain::{DocumentId, GenerationId};

use super::vectors;
use crate::{
    rocks::{GraphSnapshot, ReadBudget, StateNamespace, StateTransaction},
    semantic::{
        EmbeddingModelSelector, SemanticReadinessReport, SemanticStorageError as Error,
        VectorSearchHit,
    },
};

#[cfg(test)]
mod tests;

const MAXIMUM_CORPUS_BYTES: usize = 64 * 1_048_576;

#[derive(Clone, PartialEq, Eq)]
struct Fence {
    generation: Option<GenerationId>,
    model: EmbeddingModelSelector,
    epochs: [Option<Vec<u8>>; 2],
}

impl Fence {
    fn capture(
        transaction: &StateTransaction<'_>,
        model: &EmbeddingModelSelector,
        budget: &mut ReadBudget,
    ) -> Result<Self, Error> {
        let mut epochs = [None, None];
        for (value, key) in epochs.iter_mut().zip(super::materialize::INPUT_KEYS) {
            *value = transaction.get(StateNamespace::Metadata, key, budget)?;
            if value.as_ref().is_some_and(|value| value.len() != 8) {
                return Err(vectors::corrupt());
            }
        }
        Ok(Self {
            generation: transaction.current()?.map(|current| current.generation_id),
            model: model.clone(),
            epochs,
        })
    }
}

struct Corpus {
    fence: Fence,
    documents: Vec<DocumentId>,
    values: Vec<f32>,
    norms: Vec<f64>,
}

#[derive(Default)]
struct Cached {
    readiness: Option<(Fence, SemanticReadinessReport)>,
    corpus: Option<Arc<Corpus>>,
    building: bool,
}

/// Clones of the native service share this finite optional acceleration state.
#[derive(Default)]
pub(crate) struct SemanticCache(Mutex<Cached>);

struct BuildPermit<'a>(&'a SemanticCache);
impl Drop for BuildPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut cache) = self.0.0.lock() {
            cache.building = false;
        }
    }
}

// Field order keeps the permit until every partial corpus allocation is freed.
struct Packing<'a, T = Corpus> {
    corpus: T,
    permit: BuildPermit<'a>,
}

pub(super) fn readiness(
    cache: &SemanticCache,
    transaction: &StateTransaction<'_>,
    model: &EmbeddingModelSelector,
    budget: &mut ReadBudget,
) -> Result<SemanticReadinessReport, Error> {
    budget.checkpoint()?;
    let fence = Fence::capture(transaction, model, budget)?;
    if let Ok(cached) = cache.0.lock()
        && let Some((key, report)) = &cached.readiness
        && key == &fence
    {
        return Ok(report.clone());
    }
    let mut packing = None;
    let mut attempted = false;
    let report = vectors::readiness(transaction, model, budget, |documents, id, vector| {
        if !attempted {
            packing = begin_build(cache, fence.clone(), documents, model.dimension)?;
            attempted = true;
        }
        if let Some(packing) = &mut packing {
            packing.corpus.push(id, &vector)?;
        }
        Ok(())
    })?;
    budget.checkpoint()?;
    if report.ready()
        && packing.as_ref().is_some_and(|packing| {
            u64::try_from(packing.corpus.documents.len()).ok() != Some(report.documents())
        })
    {
        return Err(vectors::corrupt());
    }
    let (_permit, corpus) = packing.map_or((None, None), |Packing { corpus, permit }| {
        (Some(permit), report.ready().then(|| Arc::new(corpus)))
    });
    if let Ok(mut cached) = cache.0.lock() {
        cached.readiness = Some((fence, report.clone()));
        if let Some(corpus) = corpus {
            cached.corpus = Some(corpus);
        }
    }
    Ok(report)
}

#[derive(Clone, Copy)]
pub(super) struct VectorQuery<'a, 'view, 'store> {
    pub(super) transaction: &'a StateTransaction<'store>,
    pub(super) graph: &'a GraphSnapshot<'view, 'store>,
    pub(super) generation: &'a GenerationId,
    pub(super) model: &'a EmbeddingModelSelector,
    pub(super) vector: &'a [f32],
    pub(super) limit: usize,
}

pub(super) fn top_k(
    cache: &SemanticCache,
    query: VectorQuery<'_, '_, '_>,
    budget: &mut ReadBudget,
) -> Result<Vec<VectorSearchHit>, Error> {
    let VectorQuery {
        transaction,
        graph,
        generation,
        model,
        vector,
        limit,
    } = query;
    let fence = Fence::capture(transaction, model, budget)?;
    let cached = cache.0.lock().ok().and_then(|cached| {
        cached
            .corpus
            .as_ref()
            .filter(|corpus| corpus.fence == fence)
            .cloned()
    });
    let corpus = match cached {
        Some(corpus) => Some(corpus),
        None => build(cache, &query, fence, budget)?,
    };
    let Some(corpus) = corpus else {
        return vectors::top_k(transaction, graph, generation, model, vector, limit, budget);
    };
    let dimension = usize::from(model.dimension);
    let query_norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let mut selected = Vec::new();
    for ((document, candidate), norm) in corpus
        .documents
        .iter()
        .zip(corpus.values.chunks_exact(dimension))
        .zip(&corpus.norms)
    {
        budget.checkpoint()?;
        let mut dot = 0_f64;
        for (index, (left, right)) in vector.iter().zip(candidate).enumerate() {
            if index % 128 == 0 {
                budget.checkpoint()?;
            }
            dot += f64::from(*left) * f64::from(*right);
        }
        let distance = 1.0 - dot / (query_norm * norm);
        if !distance.is_finite() || !(-1e-9..=2.0 + 1e-9).contains(&distance) {
            return Err(vectors::corrupt());
        }
        crate::retrieval::native::retain(
            &mut selected,
            (document.clone(), distance.clamp(0.0, 2.0)),
            limit,
            |a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)),
        );
    }
    selected
        .into_iter()
        .map(|(id, distance)| vectors::hit(graph, generation, &id, distance, budget))
        .collect()
}

fn build(
    cache: &SemanticCache,
    query: &VectorQuery<'_, '_, '_>,
    fence: Fence,
    budget: &mut ReadBudget,
) -> Result<Option<Arc<Corpus>>, Error> {
    let VectorQuery {
        transaction,
        graph,
        generation,
        model,
        ..
    } = *query;
    let Some(mut packing) = begin_build(cache, fence, graph.info().documents, model.dimension)?
    else {
        return Ok(None);
    };
    let normalization = super::vectors::active(transaction, model, budget)?.normalization;
    vectors::visit_vectors(
        transaction,
        graph,
        generation,
        model,
        budget,
        |id, candidate, _| {
            crate::semantic::common::validate_normalization(&candidate, normalization)?;
            packing.corpus.push(id, &candidate)
        },
    )?;
    if u64::try_from(packing.corpus.documents.len()).ok() != Some(graph.info().documents) {
        return Err(Error::NotReady {
            state: crate::SemanticReadinessState::CoverageIncomplete,
        });
    }
    budget.checkpoint()?;
    let corpus = Arc::new(packing.corpus);
    if let Ok(mut cached) = cache.0.lock() {
        cached.corpus = Some(corpus.clone());
    }
    Ok(Some(corpus))
}

fn begin_build(
    cache: &SemanticCache,
    fence: Fence,
    documents: u64,
    dimension: u16,
) -> Result<Option<Packing<'_>>, Error> {
    let documents = usize::try_from(documents).map_err(|_| vectors::corrupt())?;
    let dimensions = usize::from(dimension);
    let values = documents
        .checked_mul(dimensions)
        .ok_or(vectors::corrupt())?;
    let bytes = values.checked_mul(size_of::<f32>()).and_then(|bytes| {
        documents
            .checked_mul(size_of::<DocumentId>() + 64 + size_of::<f64>())
            .and_then(|ids| bytes.checked_add(ids))
    });
    if documents == 0 || bytes.is_none_or(|bytes| bytes > MAXIMUM_CORPUS_BYTES) {
        return Ok(None);
    }
    {
        let Ok(mut cached) = cache.0.lock() else {
            return Ok(None);
        };
        // Do not build a replacement while another query retains the old
        // allocation, or while another native worker is already building it.
        if cached.building
            || cached
                .corpus
                .as_ref()
                .is_some_and(|old| Arc::strong_count(old) != 1)
        {
            return Ok(None);
        }
        cached.corpus = None;
        cached.building = true;
    }
    let permit = BuildPermit(cache);
    let Some(corpus) = allocate(fence, documents, values)? else {
        return Ok(None);
    };
    Ok(Some(Packing { corpus, permit }))
}

impl Corpus {
    fn push(&mut self, id: DocumentId, candidate: &[f32]) -> Result<(), Error> {
        if self.documents.len() == self.documents.capacity()
            || self.norms.len() == self.norms.capacity()
            || self
                .values
                .len()
                .checked_add(candidate.len())
                .is_none_or(|values| values > self.values.capacity())
        {
            return Err(workspace());
        }
        self.norms.push(
            candidate
                .iter()
                .map(|value| f64::from(*value).powi(2))
                .sum::<f64>()
                .sqrt(),
        );
        self.values.extend_from_slice(candidate);
        self.documents.push(id);
        Ok(())
    }
}

fn workspace() -> Error {
    Error::InvalidInput {
        field: "semantic_corpus_workspace",
    }
}

fn allocate(fence: Fence, documents: usize, values: usize) -> Result<Option<Corpus>, Error> {
    let mut corpus = Corpus {
        fence,
        documents: Vec::new(),
        values: Vec::new(),
        norms: Vec::new(),
    };
    corpus
        .documents
        .try_reserve_exact(documents)
        .map_err(|_| workspace())?;
    corpus
        .values
        .try_reserve_exact(values)
        .map_err(|_| workspace())?;
    corpus
        .norms
        .try_reserve_exact(documents)
        .map_err(|_| workspace())?;
    let retained = corpus
        .values
        .capacity()
        .checked_mul(size_of::<f32>())
        .and_then(|bytes| {
            corpus
                .documents
                .capacity()
                .checked_mul(size_of::<DocumentId>() + 64)
                .and_then(|ids| bytes.checked_add(ids))
        })
        .and_then(|bytes| {
            corpus
                .norms
                .capacity()
                .checked_mul(size_of::<f64>())
                .and_then(|norms| bytes.checked_add(norms))
        });
    if retained.is_none_or(|bytes| bytes > MAXIMUM_CORPUS_BYTES) {
        return Ok(None);
    }
    Ok(Some(corpus))
}
