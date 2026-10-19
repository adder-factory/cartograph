//! Propagated summaries require exact source bodies and never form propagation chains.

use super::backlog::evidence;
use super::summaries::{current_symbol_summary, summary_kind};
use super::{Kind, Record, Scope, State, WRITE_BYTES, WRITE_ROWS, workspace, write};
use crate::{
    NativeDatabase, NewAgentArtifact, StorageError,
    artifacts::{
        NeighborSummarySaveRequest, NeighborSummarySource, PendingNeighborSummary,
        PendingNeighborSummaryQuery, validate_limit,
    },
    retrieval::native::{retain, visit_symbols},
    rocks::StateNamespace,
    semantic::{
        SemanticStorageError,
        native::{embedded_symbols, feature_error},
    },
};
use cartograph_domain::{GenerationId, ProjectId, SymbolId};
use std::collections::BTreeSet;

impl NativeDatabase {
    /// Page embedded current symbols with no exact complete summary.
    /// # Errors
    /// Rejects stale generation, invalid model/bounds, and exhausted native reads.
    pub async fn pending_neighbor_summaries(
        &self,
        request: PendingNeighborSummaryQuery<'_>,
    ) -> Result<Vec<PendingNeighborSummary>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, super::super::MAX_NEIGHBOR_SUMMARY_BATCH)?;
        let (generation, model, after, limit) = (
            request.expected_generation_id.clone(),
            request.model_id.clone(),
            request.after_symbol_id.cloned(),
            usize::from(request.limit),
        );
        self.transaction(
            Some(generation.clone()),
            0,
            0,
            super::super::ARTIFACT_TIMEOUT,
            move |transaction, budget| -> Result<_, SemanticStorageError> {
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let embedded = embedded_symbols(transaction, &graph, &model, budget)?;
                let mut selected = Vec::new();
                visit_symbols(&graph, budget, |symbol, budget| {
                    if summary_kind(&symbol.symbol_kind)
                        && embedded.contains(&symbol.symbol_id)
                        && after.as_ref().is_none_or(|after| &symbol.symbol_id > after)
                        && current_symbol_summary(transaction, &symbol, budget)?.is_none()
                    {
                        retain(&mut selected, symbol, limit, |a, b| {
                            a.symbol_id.cmp(&b.symbol_id)
                        });
                        if selected
                            .iter()
                            .map(|symbol| {
                                symbol.qualified_name.len() + symbol.signature.len() + 512
                            })
                            .sum::<usize>()
                            > 16 * 1_048_576
                        {
                            return Err(workspace());
                        }
                    }
                    Ok(())
                })?;
                let ids = selected
                    .iter()
                    .map(|symbol| symbol.symbol_id.clone())
                    .collect();
                let sources = evidence(&graph, budget, &ids)?;
                Ok(selected
                    .into_iter()
                    .map(|symbol| PendingNeighborSummary {
                        generation_id: generation.to_string(),
                        symbol_id: symbol.symbol_id.to_string(),
                        symbol_kind: symbol.symbol_kind,
                        name: sources
                            .get(&symbol.symbol_id)
                            .map_or(symbol.qualified_name, |source| source.name.clone()),
                        content_hash: symbol.structural_digest.to_string(),
                    })
                    .collect())
            },
        )
        .await
        .map_err(feature_error)
    }

    /// Read exact non-propagated summaries for a bounded neighbor candidate set.
    /// # Errors
    /// Rejects stale generation, invalid sets, and exhausted native reads.
    pub async fn neighbor_summary_sources(
        &self,
        project: &ProjectId,
        generation: &GenerationId,
        symbols: &[SymbolId],
    ) -> Result<Vec<NeighborSummarySource>, StorageError> {
        self.require_project(project)?;
        if symbols.is_empty() || symbols.len() > super::super::MAX_NEIGHBOR_SUMMARY_SOURCES {
            return Err(StorageError::InvalidInput {
                field: "neighbor_summary_sources",
            });
        }
        let ids = symbols.iter().cloned().collect::<BTreeSet<_>>();
        self.state(
            Some(generation.clone()),
            0,
            0,
            move |transaction, budget| {
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let mut sources = evidence(&graph, budget, &ids)?;
                let mut output = Vec::new();
                let mut bytes = 0_usize;
                for id in ids {
                    let Some(symbol) = graph.symbol(&id, budget)? else {
                        continue;
                    };
                    let Some(summary) = current_symbol_summary(transaction, &symbol, budget)?
                    else {
                        continue;
                    };
                    let model = producer(&summary);
                    if model.starts_with("neighbor:") {
                        continue;
                    }
                    bytes = bytes
                        .checked_add(summary.body.len() + symbol.qualified_name.len() + 1024)
                        .ok_or_else(workspace)?;
                    if bytes > 32 * 1_048_576 {
                        return Err(workspace());
                    }
                    output.push(NeighborSummarySource {
                        symbol_id: id.to_string(),
                        symbol_kind: symbol.symbol_kind,
                        name: sources
                            .remove(&id)
                            .map_or(symbol.qualified_name, |source| source.name),
                        model: model.to_owned(),
                        summary: summary.body,
                    });
                }
                Ok(output)
            },
        )
        .await
    }

    /// Revalidate the target digest, neighbor kind, and echoed source body before saving.
    /// # Errors
    /// Rejects invalid projects and native transaction failure; stale evidence returns no save.
    pub async fn save_neighbor_symbol_summary(
        &self,
        project: &ProjectId,
        request: NeighborSummarySaveRequest<'_>,
    ) -> Result<Option<Record>, StorageError> {
        self.require_project(project)?;
        let (id, digest, neighbor, source_body, body, model, similarity) = (
            request.symbol_id.clone(),
            request.source_digest.clone(),
            request.neighbor_symbol_id.clone(),
            request.neighbor_summary.to_owned(),
            request.summary.to_owned(),
            request.embedding_model_id.clone(),
            request.similarity,
        );
        self.state(None, WRITE_BYTES, WRITE_ROWS, move |transaction, budget| {
            let Some(current) = transaction.current()? else {return Ok(None);};
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let Some(target) = graph.symbol(&id, budget)? else {return Ok(None);};
            let Some(source) = graph.symbol(&neighbor, budget)? else {return Ok(None);};
            if target.symbol_kind != source.symbol_kind || target.structural_digest != digest
                || current_symbol_summary(transaction, &target, budget)?.is_some() {return Ok(None);}
            let Some(summary) = current_symbol_summary(transaction, &source, budget)? else {return Ok(None);};
            if summary.body != source_body || producer(&summary).starts_with("neighbor:") {return Ok(None);}
            let metadata = serde_json::json!({"model":format!("neighbor:v2:{model}"),
                "generationMode":"semantic_neighbor", "sourceSymbolId":neighbor.as_str(),
                "sourceModel":producer(&summary), "embeddingModelId":model.as_str(), "similarity":similarity});
            drop(graph);
            drop(snapshot);
            let saved = write(transaction, budget, NewAgentArtifact {kind:Kind::Summary, scope:Scope::Symbol,
                scope_key:id.to_string(), body, metadata, generation_id:Some(current.generation_id.clone()),
                source_digest:Some(digest), state:State::Complete}, true)?;
            transaction.delete(StateNamespace::Derived, &crate::summary_priority::native::key(&current.generation_id, &id))?;
            Ok(Some(saved))
        }).await
    }
}

fn producer(record: &Record) -> &str {
    record
        .metadata
        .get("model")
        .and_then(serde_json::Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or("agent")
}
