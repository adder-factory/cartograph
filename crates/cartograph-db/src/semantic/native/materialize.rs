//! Bounded exact similarity builds publish only after vector and summary fences pass.

use super::{
    NAMESPACE,
    vectors::{cosine_distance, current, documents_scan, read_vector, source},
};
use crate::{
    NativeDatabase,
    native_database::{
        dataset::{self, CommitGuard},
        visit_state,
    },
    retrieval::native::retain,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateNamespace, StateTransaction},
    semantic::{
        EmbeddingModelSelector, EmbeddingModelState, RegisteredEmbeddingModel,
        SemanticStorageError as Error, SimilarityMaterializationPolicy,
        SimilarityMaterializationReport,
    },
};
use cartograph_domain::{DocumentKind, GenerationId, ModelId, ProjectId, SymbolId};
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, ops::ControlFlow};

pub(crate) const INPUT_KEYS: &[&[u8]] = &[b"vector_epoch", b"summary_epoch"];

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SimilarityEdge {
    pub(crate) model: ModelId,
    pub(crate) source: SymbolId,
    pub(crate) target: SymbolId,
    pub(crate) score: f64,
    rank: u16,
}
#[derive(Serialize, Deserialize)]
struct Metadata {
    report: SimilarityMaterializationReport,
    guard: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}
struct Source {
    symbol: SymbolId,
    vector: Vec<f32>,
}
struct Build {
    generation: GenerationId,
    guard: CommitGuard,
    report: SimilarityMaterializationReport,
    rows: Vec<SimilarityEdge>,
}

impl NativeDatabase {
    /// Build exact current model neighbors and atomically replace the derived cache.
    /// # Errors
    /// Preserves the previous complete build on changed inputs, deadlines, or workspace limits.
    pub async fn rebuild_current_similarity_edges(
        &self,
        project: &ProjectId,
        policy: SimilarityMaterializationPolicy,
    ) -> Result<SimilarityMaterializationReport, Error> {
        self.require_project(project)?;
        let build = self
            .transaction(
                None,
                0,
                0,
                policy.statement_timeout,
                move |transaction, budget| build(transaction, budget, policy),
            )
            .await?;
        let metadata = Metadata {
            report: build.report,
            guard: build
                .guard
                .0
                .iter()
                .map(|(key, value)| (key.to_vec(), value.clone()))
                .collect(),
        };
        self.replace_dataset_guarded(
            StateNamespace::Derived,
            "similarity",
            Some(build.generation),
            &metadata,
            build.rows.into_iter(),
            build.guard,
        )
        .await?;
        Ok(build.report)
    }
}

fn build(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    policy: SimilarityMaterializationPolicy,
) -> Result<Build, Error> {
    let generation = current(transaction)?;
    let guard = CommitGuard::capture(transaction, INPUT_KEYS, budget)?;
    let mut models = Vec::new();
    visit_state(
        transaction,
        NAMESPACE,
        b"m/",
        budget,
        |_, model: RegisteredEmbeddingModel, _| {
            if model.state == EmbeddingModelState::Active {
                models.push(model);
            }
            if models.len() > 256 {
                return Err(RocksError::Limit {
                    resource: "embedding_models",
                });
            }
            Ok(())
        },
    )?;
    let snapshot = transaction.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let mut report = SimilarityMaterializationReport {
        models: 0,
        source_symbols: 0,
        model_symbol_pairs: 0,
        edges_written: 0,
        neighbors_per_symbol: policy.neighbors,
        minimum_score_millionths: (policy.minimum_score * 1_000_000.0)
            .round()
            .to_u32()
            .ok_or(Error::InvalidInput {
                field: "similarity_score",
            })?,
    };
    let mut distinct = BTreeSet::new();
    let mut rows = Vec::new();
    for model in models {
        let sources = model_sources(transaction, &graph, budget, &generation, &model.selector)?;
        if sources.is_empty() {
            continue;
        }
        report.models += 1;
        report.model_symbol_pairs = report
            .model_symbol_pairs
            .checked_add(sources.len() as u64)
            .ok_or(workspace())?;
        if report.model_symbol_pairs > 100_000 {
            return Err(Error::InvalidInput {
                field: "similarity_source_symbols",
            });
        }
        distinct.extend(sources.iter().map(|source| source.symbol.clone()));
        append_neighbors(
            &sources,
            &model.selector.model_id,
            policy,
            budget,
            &mut rows,
        )?;
    }
    report.source_symbols = distinct.len() as u64;
    report.edges_written = rows.len() as u64;
    Ok(Build {
        generation,
        guard,
        report,
        rows,
    })
}

fn model_sources(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    generation: &GenerationId,
    model: &EmbeddingModelSelector,
) -> Result<Vec<Source>, Error> {
    let mut sources = Vec::new();
    let mut bytes = 0_usize;
    documents_scan(graph, budget, |_, document, budget| {
        if document.kind == DocumentKind::Symbol
            && let Some(symbol) = document.symbol_id.clone()
        {
            let id = document.document_id.clone();
            let (_, digest) = source(transaction, graph, generation, document, budget)?;
            if let Some(vector) = read_vector(transaction, model, &id, &digest, budget)? {
                bytes = bytes
                    .checked_add(vector.len() * 4 + symbol.as_str().len() + 128)
                    .ok_or(workspace())?;
                if bytes > 32 * 1_048_576 {
                    return Err(workspace());
                }
                sources.push(Source { symbol, vector });
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    sources.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    if sources
        .windows(2)
        .any(|pair| pair[0].symbol == pair[1].symbol)
    {
        return Err(Error::CorruptStoredValue {
            field: "similarity_source",
        });
    }
    Ok(sources)
}

fn append_neighbors(
    sources: &[Source],
    model: &ModelId,
    policy: SimilarityMaterializationPolicy,
    budget: &mut ReadBudget,
    rows: &mut Vec<SimilarityEdge>,
) -> Result<(), Error> {
    for source in sources {
        let mut neighbors = Vec::new();
        for target in sources {
            budget.checkpoint()?;
            if source.symbol == target.symbol {
                continue;
            }
            let score = 1.0 - cosine_distance(&source.vector, &target.vector, budget)?;
            if score >= policy.minimum_score {
                retain(
                    &mut neighbors,
                    (&target.symbol, score.clamp(0.0, 1.0)),
                    usize::from(policy.neighbors),
                    |a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)),
                );
            }
        }
        for (index, (target, score)) in neighbors.into_iter().enumerate() {
            if rows.len() >= 16 * 1_048_576 / 256 {
                return Err(workspace());
            }
            rows.push(SimilarityEdge {
                model: model.clone(),
                source: source.symbol.clone(),
                target: target.clone(),
                score,
                rank: u16::try_from(index + 1).map_err(|_| workspace())?,
            });
        }
    }
    Ok(())
}

/// Visit only a current generation build whose exact vector/summary epochs still match.
pub(crate) fn visit_current(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(SimilarityEdge, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let Some(current) = transaction.current()? else {
        return Ok(());
    };
    let Some(manifest) =
        dataset::manifest(transaction, StateNamespace::Derived, "similarity", budget)?
    else {
        return Ok(());
    };
    if manifest.generation.as_ref() != Some(&current.generation_id) {
        return Ok(());
    }
    let metadata: Metadata =
        serde_json::from_value(manifest.metadata.clone()).map_err(|_| RocksError::Corrupt {
            field: "similarity_metadata",
        })?;
    let guard = CommitGuard::capture(transaction, INPUT_KEYS, budget)?;
    let actual = guard
        .0
        .iter()
        .map(|(key, value)| (key.to_vec(), value.clone()))
        .collect::<Vec<_>>();
    if metadata.guard != actual {
        return Ok(());
    }
    dataset::visit(
        transaction,
        StateNamespace::Derived,
        "similarity",
        &manifest,
        budget,
        |row: SimilarityEdge, budget| {
            if !row.score.is_finite()
                || !(0.0..=1.0).contains(&row.score)
                || row.source == row.target
                || row.rank == 0
                || row.rank > 50
            {
                return Err(RocksError::Corrupt {
                    field: "similarity_edge",
                });
            }
            visit(row, budget)
        },
    )
}
fn workspace() -> Error {
    Error::InvalidInput {
        field: "similarity_materialization_workspace",
    }
}
