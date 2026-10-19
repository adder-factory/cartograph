//! Bounded source and topology evidence for the model-free and role passes.

use super::summaries::{
    current_symbol_summary, documentation_lengths, summary_kind, visit_documents,
};
use super::{Kind, Scope, State, corrupt, read_scope, workspace};
use crate::{
    GraphDirection, NativeDatabase, StorageError, SymbolInput,
    artifacts::{
        PendingRoleSymbol, PendingStructuralSummary, PendingStructuralSummaryQuery, RoleSweepModel,
        StructuralSummaryEdge, validate_limit, validate_model,
    },
    retrieval::native::{retain, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction},
};
use cartograph_domain::{DocumentKind, SymbolId, Visibility};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

#[derive(Default)]
pub(super) struct Evidence {
    pub(super) name: String,
    pub(super) code: String,
    pub(super) natural: String,
}

pub(super) fn evidence(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    ids: &BTreeSet<SymbolId>,
) -> Result<BTreeMap<SymbolId, Evidence>, RocksError> {
    let mut output = BTreeMap::new();
    if ids.is_empty() {
        return Ok(output);
    }
    let mut bytes = 0_usize;
    visit_documents(graph, budget, |document, _| {
        if document.kind != DocumentKind::Symbol {
            return Ok(());
        }
        let Some(id) = document.symbol_id else {
            return Ok(());
        };
        if !ids.contains(&id) {
            return Ok(());
        }
        let metadata: serde_json::Value =
            serde_json::from_str(&document.metadata_json).map_err(|_| corrupt())?;
        let item = Evidence {
            name: metadata
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(&document.qualified_name)
                .to_owned(),
            code: document.code.chars().take(2048).collect(),
            natural: document.natural_text.chars().take(2048).collect(),
        };
        bytes = bytes
            .checked_add(item.name.len() + item.code.len() + item.natural.len() + 256)
            .ok_or_else(workspace)?;
        if bytes > 32 * 1_048_576 || output.len() >= 16_384 {
            return Err(workspace());
        }
        if output.insert(id, item).is_some() {
            return Err(corrupt());
        }
        Ok(())
    })?;
    Ok(output)
}

impl NativeDatabase {
    /// Page unsummarized declarations with bounded source and exact outgoing evidence.
    /// # Errors
    /// Rejects invalid bounds, corrupt facts, or exhausted native read/workspace budgets.
    pub async fn pending_structural_summaries(
        &self,
        request: PendingStructuralSummaryQuery<'_>,
    ) -> Result<Vec<PendingStructuralSummary>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, super::super::MAX_STRUCTURAL_SUMMARY_BATCH)?;
        let (after, limit, threshold) = (
            request.after_symbol_id.cloned(),
            usize::from(request.limit),
            request.policy.existing_docstring_char_threshold as usize,
        );
        self.state(None, 0, 0, move |transaction, budget| {
            let Some(current) = transaction.current()? else {
                return Ok(Vec::new());
            };
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let selected = select_structural_symbols(
                transaction,
                &graph,
                budget,
                after.as_ref(),
                limit,
                threshold,
            )?;
            let (mut edges, wanted) = structural_edges(transaction, &graph, budget, &selected)?;
            let mut documents = evidence(&graph, budget, &wanted)?;
            hydrate_structural_edges(&mut edges, &documents)?;
            let mut output = Vec::new();
            for symbol in selected {
                let source = documents.remove(&symbol.symbol_id).unwrap_or_default();
                let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                output.push(PendingStructuralSummary {
                    generation_id: current.generation_id.to_string(),
                    symbol_id: symbol.symbol_id.to_string(),
                    path: file.normalized_path,
                    symbol_kind: symbol.symbol_kind,
                    name: if source.name.is_empty() {
                        symbol.qualified_name.clone()
                    } else {
                        source.name
                    },
                    qualified_name: symbol.qualified_name,
                    signature: symbol.signature.chars().take(1024).collect(),
                    code: source.code,
                    start_line: symbol.start_line,
                    end_line: symbol.end_line,
                    content_hash: symbol.structural_digest.to_string(),
                    declaration_only: symbol.declaration_only,
                    edges: edges.remove(&symbol.symbol_id).unwrap_or_default(),
                });
            }
            Ok(output)
        })
        .await
    }

    /// Page roles needing the exact configured producer without downgrading judged roles.
    /// # Errors
    /// Rejects invalid model/bounds, corrupt source, or exhausted native read budgets.
    pub async fn pending_symbol_roles(
        &self,
        project: &cartograph_domain::ProjectId,
        sweep: RoleSweepModel<'_>,
        limit: u16,
    ) -> Result<Vec<PendingRoleSymbol>, StorageError> {
        self.require_project(project)?;
        validate_limit(limit, super::super::MAX_ROLE_BATCH)?;
        let (model, keep_judged) = match sweep {
            RoleSweepModel::Structural(model) => (model, true),
            RoleSweepModel::Judge(model) => (model, false),
        };
        validate_model(model)?;
        let model = model.to_owned();
        self.state(None, 0, 0, move |transaction, budget| {
            if transaction.current()?.is_none() {
                return Ok(Vec::new());
            }
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let mut selected = Vec::new();
            visit_symbols(&graph, budget, |symbol, budget| {
                if matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter") {
                    return Ok(());
                }
                let role = read_scope(
                    transaction,
                    Kind::Role,
                    Scope::Symbol,
                    symbol.symbol_id.as_str(),
                    budget,
                )?;
                if role.is_some_and(|record| role_satisfied(&record, &symbol, &model, keep_judged))
                {
                    return Ok(());
                }
                let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                retain(
                    &mut selected,
                    (symbol, file),
                    usize::from(limit),
                    role_order,
                );
                if selected
                    .iter()
                    .map(|(symbol, file)| {
                        symbol.signature.len()
                            + symbol.qualified_name.len()
                            + file.normalized_path.len()
                            + 1024
                    })
                    .sum::<usize>()
                    > 16 * 1_048_576
                {
                    return Err(workspace());
                }
                Ok(())
            })?;
            let wanted = selected
                .iter()
                .map(|(symbol, _)| symbol.symbol_id.clone())
                .collect();
            let mut documents = evidence(&graph, budget, &wanted)?;
            selected
                .into_iter()
                .map(|(symbol, file)| {
                    let source = documents.remove(&symbol.symbol_id).unwrap_or_default();
                    let summary = current_symbol_summary(transaction, &symbol, budget)?;
                    Ok(PendingRoleSymbol {
                        symbol_id: symbol.symbol_id.to_string(),
                        path: file.normalized_path,
                        language: file.language,
                        symbol_kind: symbol.symbol_kind,
                        qualified_name: symbol.qualified_name,
                        signature: symbol.signature.chars().take(1024).collect(),
                        description: summary
                            .filter(|summary| !summary.body.is_empty())
                            .map_or(source.natural, |summary| summary.body),
                        code: source.code,
                        exported: symbol.export.exported,
                    })
                })
                .collect()
        })
        .await
    }
}

fn select_structural_symbols(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    after: Option<&SymbolId>,
    limit: usize,
    threshold: usize,
) -> Result<Vec<SymbolInput>, RocksError> {
    let lengths = documentation_lengths(graph, budget)?;
    let mut selected = Vec::new();
    visit_symbols(graph, budget, |symbol, budget| {
        if summary_kind(&symbol.symbol_kind)
            && after.is_none_or(|after| &symbol.symbol_id > after)
            && lengths.get(&symbol.symbol_id).copied().unwrap_or(0) < threshold
            && current_symbol_summary(transaction, &symbol, budget)?.is_none()
        {
            retain(&mut selected, symbol, limit, |a, b| {
                a.symbol_id.cmp(&b.symbol_id)
            });
            check_symbols(&selected)?;
        }
        Ok(())
    })?;
    Ok(selected)
}

type SummaryEdges = BTreeMap<SymbolId, Vec<StructuralSummaryEdge>>;

fn structural_edges(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    selected: &[SymbolInput],
) -> Result<(SummaryEdges, BTreeSet<SymbolId>), RocksError> {
    let mut wanted = selected
        .iter()
        .map(|symbol| symbol.symbol_id.clone())
        .collect::<BTreeSet<_>>();
    let mut edges = BTreeMap::new();
    let mut bytes = 0_usize;
    for symbol in selected {
        let links = summary_edges(transaction, graph, budget, &symbol.symbol_id)?;
        for edge in &links {
            wanted.insert(SymbolId::parse(&edge.target_symbol_id).map_err(|_| corrupt())?);
            bytes = bytes
                .checked_add(
                    edge.target_path.len()
                        + edge.target_name.len()
                        + edge.target_qualified_name.len()
                        + edge.target_summary.as_ref().map_or(0, String::len)
                        + 256,
                )
                .ok_or_else(workspace)?;
            if bytes > 16 * 1_048_576 || wanted.len() > 16_384 {
                return Err(workspace());
            }
        }
        edges.insert(symbol.symbol_id.clone(), links);
    }
    Ok((edges, wanted))
}

fn hydrate_structural_edges(
    edges: &mut SummaryEdges,
    documents: &BTreeMap<SymbolId, Evidence>,
) -> Result<(), RocksError> {
    for links in edges.values_mut() {
        for edge in links {
            let id = SymbolId::parse(&edge.target_symbol_id).map_err(|_| corrupt())?;
            if let Some(source) = documents.get(&id) {
                edge.target_name.clone_from(&source.name);
                if edge.target_summary.is_none() && !source.natural.is_empty() {
                    edge.target_summary = Some(source.natural.chars().take(600).collect());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn role_satisfied(
    record: &super::Record,
    symbol: &SymbolInput,
    model: &str,
    keep_judged: bool,
) -> bool {
    let via = record
        .metadata
        .get("via")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let producer = record
        .metadata
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    record.state == State::Complete
        && record.source_digest.as_deref() == Some(symbol.structural_digest.as_str())
        && (matches!(via, "structural_rule" | "rule")
            || (matches!(via, "structural_fallback" | "llm" | "jev") && producer == model)
            || (keep_judged && matches!(via, "llm" | "jev")))
}
fn role_order(
    a: &(SymbolInput, crate::FileInput),
    b: &(SymbolInput, crate::FileInput),
) -> Ordering {
    b.0.export
        .exported
        .cmp(&a.0.export.exported)
        .then(
            (b.0.visibility == Some(Visibility::Public))
                .cmp(&(a.0.visibility == Some(Visibility::Public))),
        )
        .then(a.1.normalized_path.cmp(&b.1.normalized_path))
        .then(a.0.start_line.cmp(&b.0.start_line))
        .then(a.0.symbol_id.cmp(&b.0.symbol_id))
}
fn check_symbols(symbols: &[SymbolInput]) -> Result<(), RocksError> {
    if symbols
        .iter()
        .map(|symbol| symbol.signature.len() + symbol.qualified_name.len() + 1024)
        .sum::<usize>()
        > 16 * 1_048_576
    {
        return Err(workspace());
    }
    Ok(())
}
fn summary_edges(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    id: &SymbolId,
) -> Result<Vec<StructuralSummaryEdge>, RocksError> {
    let mut output = Vec::new();
    let mut cursor = None;
    let mut bytes = 0_usize;
    loop {
        let page = graph.neighbors(id, GraphDirection::Outgoing, cursor.as_ref(), 256, budget)?;
        for link in page.links {
            budget.checkpoint()?;
            let symbol = graph
                .symbol(link.target_symbol_id(), budget)?
                .ok_or(corrupt())?;
            let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
            let summary = current_symbol_summary(transaction, &symbol, budget)?
                .map(|summary| summary.body.chars().take(600).collect::<String>());
            bytes = bytes
                .checked_add(
                    symbol.qualified_name.len() * 2
                        + file.normalized_path.len()
                        + summary.as_ref().map_or(0, String::len)
                        + 256,
                )
                .ok_or_else(workspace)?;
            if bytes > 16 * 1_048_576 || output.len() >= 16_384 {
                return Err(workspace());
            }
            output.push(StructuralSummaryEdge {
                edge_kind: link.kind().as_str().to_owned(),
                target_symbol_id: symbol.symbol_id.to_string(),
                target_kind: symbol.symbol_kind,
                target_name: symbol.qualified_name.clone(),
                target_qualified_name: symbol.qualified_name,
                target_path: file.normalized_path,
                target_summary: summary,
            });
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    output.sort_by(|a, b| {
        a.edge_kind
            .cmp(&b.edge_kind)
            .then(a.target_qualified_name.cmp(&b.target_qualified_name))
            .then(a.target_symbol_id.cmp(&b.target_symbol_id))
    });
    Ok(output)
}
