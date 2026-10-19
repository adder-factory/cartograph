//! Canonical clone classes suppress symmetric rows and preserve bounded member evidence.

use super::{
    corrupt, framework,
    inventory::{Base, Document},
    rules, workspace,
};
use crate::{
    insights::StructuralFinding,
    retrieval::native::retain,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction},
    semantic::native::materialize,
};
use cartograph_domain::SymbolId;
use num_traits::ToPrimitive as _;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Classes {
    representatives: BTreeMap<SymbolId, Class>,
    claimed: BTreeSet<SymbolId>,
    production: BTreeSet<SymbolId>,
}
struct Class {
    kind: &'static str,
    metric: &'static str,
    count: u64,
    members: Vec<SymbolId>,
    score: Option<f64>,
}
type Groups = BTreeMap<[u8; 32], Vec<SymbolId>>;
type SemanticLinks = BTreeMap<SymbolId, BTreeMap<SymbolId, f64>>;

impl Classes {
    pub(super) fn new(
        transaction: &StateTransaction<'_>,
        graph: &GraphSnapshot<'_, '_>,
        documents: &BTreeMap<SymbolId, Document>,
        budget: &mut ReadBudget,
    ) -> Result<Self, RocksError> {
        let mut output = Self {
            representatives: BTreeMap::new(),
            claimed: BTreeSet::new(),
            production: BTreeSet::new(),
        };
        let mut exact = Groups::new();
        let mut bytes = 0_usize;
        for (id, document) in documents {
            budget.checkpoint()?;
            if document.test || !document.clones.enabled {
                continue;
            }
            let symbol = graph.symbol(id, budget)?.ok_or(corrupt())?;
            if !matches!(
                symbol.symbol_kind.as_str(),
                "function" | "method" | "component"
            ) {
                continue;
            }
            let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
            if !framework::production(&file.normalized_path)? {
                continue;
            }
            output.production.insert(id.clone());
            bytes = bytes.checked_add(128).ok_or_else(workspace)?;
            if symbol.end_line - symbol.start_line + 1 >= 6 {
                let hash = blake3::Hash::from_hex(symbol.structural_digest.as_str())
                    .map_err(|_| corrupt())?;
                add_group(&mut exact, *hash.as_bytes(), id, &mut bytes)?;
            }
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
        }
        output.absorb(exact, "exact", "exact_copies")?;
        let near = output.near_groups(graph, documents, budget)?;
        output.absorb(near, "near", "normalized_shape_copies")?;
        output.semantic(transaction, graph, documents, budget)?;
        Ok(output)
    }
    fn near_groups(
        &self,
        graph: &GraphSnapshot<'_, '_>,
        documents: &BTreeMap<SymbolId, Document>,
        budget: &mut ReadBudget,
    ) -> Result<Groups, RocksError> {
        let mut groups = Groups::new();
        let mut bytes = self.bytes();
        for id in self.production.difference(&self.claimed) {
            budget.checkpoint()?;
            let document = documents.get(id).ok_or(corrupt())?;
            let Some(shape) = document
                .shape
                .as_ref()
                .filter(|_| document.clones.near_compatible)
            else {
                continue;
            };
            let symbol = graph.symbol(id, budget)?.ok_or(corrupt())?;
            let size = symbol
                .end_byte
                .saturating_sub(symbol.start_byte)
                .max(1)
                .to_f64()
                .ok_or(corrupt())?;
            if symbol.end_line - symbol.start_line + 1 < 12
                || document.health("literal_bytes") / size > 0.6
            {
                continue;
            }
            add_group(
                &mut groups,
                *blake3::hash(shape.as_bytes()).as_bytes(),
                id,
                &mut bytes,
            )?;
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
        }
        Ok(groups)
    }
    fn absorb(
        &mut self,
        groups: Groups,
        kind: &'static str,
        metric: &'static str,
    ) -> Result<(), RocksError> {
        for mut members in groups.into_values().filter(|members| members.len() > 1) {
            members.sort();
            let representative = members.first().ok_or(corrupt())?.clone();
            self.claimed.extend(members.iter().cloned());
            self.representatives.insert(
                representative,
                Class {
                    kind,
                    metric,
                    count: members.len() as u64,
                    members,
                    score: None,
                },
            );
        }
        if self.bytes() > 16 * 1_048_576 {
            return Err(workspace());
        }
        Ok(())
    }
    fn bytes(&self) -> usize {
        (self.production.len() + self.claimed.len()) * 128
            + self
                .representatives
                .values()
                .map(|class| 256 + class.members.len() * 96)
                .sum::<usize>()
    }
    fn semantic(
        &mut self,
        transaction: &StateTransaction<'_>,
        graph: &GraphSnapshot<'_, '_>,
        documents: &BTreeMap<SymbolId, Document>,
        budget: &mut ReadBudget,
    ) -> Result<(), RocksError> {
        let mut eligible = BTreeSet::new();
        for id in self.production.difference(&self.claimed) {
            let document = documents.get(id).ok_or(corrupt())?;
            let symbol = graph.symbol(id, budget)?.ok_or(corrupt())?;
            if document.partial.peers == 0 && symbol.end_line - symbol.start_line + 1 >= 6 {
                eligible.insert(id.clone());
            }
        }
        let (links, bytes) = semantic_links(
            transaction,
            budget,
            &eligible,
            self.bytes() + eligible.len() * 128,
        )?;
        let mut visited = BTreeSet::new();
        for start in links.keys() {
            budget.checkpoint()?;
            if visited.contains(start) {
                continue;
            }
            let members = connected_members(&links, start, &mut visited, budget)?;
            let representative = members.first().ok_or(corrupt())?.clone();
            let peers = links.get(&representative).ok_or(corrupt())?;
            let score = peers.values().copied().reduce(f64::max).ok_or(corrupt())?;
            self.claimed.extend(members.iter().cloned());
            self.representatives.insert(
                representative,
                Class {
                    kind: "semantic",
                    metric: "semantic_clone_peers",
                    count: peers.len() as u64,
                    members,
                    score: Some(score),
                },
            );
            if self.bytes() + bytes > 32 * 1_048_576 {
                return Err(workspace());
            }
        }
        Ok(())
    }
    pub(super) fn finding(
        &self,
        base: &Base<'_>,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<Option<StructuralFinding>, RocksError> {
        let id = &base.symbol.symbol_id;
        if let Some(class) = self.representatives.get(id) {
            let mut detail = json!({ "cloneType":class.kind, "classSize":class.members.len(), "recordScope":"clone_class",
                "symmetricMemberRowsSuppressed":true, "loc":base.symbol.end_line - base.symbol.start_line + 1,
                "members":members(class.members.iter().filter(|peer| *peer != id), graph, budget)? });
            if let Some(score) = class.score {
                detail["maximumSemanticScore"] = json!(score);
                detail["semanticThreshold"] = json!(0.95);
            }
            return Ok(Some(rules::finding(
                base,
                "duplicate_code",
                class.metric,
                class.count.to_f64().ok_or(corrupt())?,
                if class.kind == "exact" {
                    "warning"
                } else {
                    "info"
                },
                Some(detail),
            )));
        }
        let partial = &base.document.partial;
        if self.claimed.contains(id)
            || !self.production.contains(id)
            || partial.peers == 0
            || partial.representative.as_ref() != Some(id)
            || base.symbol.end_line - base.symbol.start_line + 1 < 12
        {
            return Ok(None);
        }
        let detail = json!({ "cloneType":"partial", "classSize":partial.component, "recordScope":"clone_class",
            "symmetricMemberRowsSuppressed":true, "loc":base.symbol.end_line - base.symbol.start_line + 1,
            "maximumOverlap":partial.maximum, "minimumOverlap":partial.minimum,
            "members":members(partial.listed.iter().filter(|id| self.production.contains(*id)), graph, budget)? });
        Ok(Some(rules::finding(
            base,
            "duplicate_code",
            "partial_clone_peers",
            partial.peers.to_f64().ok_or(corrupt())?,
            "info",
            Some(detail),
        )))
    }
}

fn semantic_links(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    eligible: &BTreeSet<SymbolId>,
    mut bytes: usize,
) -> Result<(SemanticLinks, usize), RocksError> {
    let mut links = SemanticLinks::new();
    materialize::visit_current(transaction, budget, |edge, _| {
        if edge.score >= 0.95 && eligible.contains(&edge.source) && eligible.contains(&edge.target)
        {
            for (source, target) in [
                (edge.source.clone(), edge.target.clone()),
                (edge.target, edge.source),
            ] {
                add_semantic_link(&mut links, &mut bytes, source, target, edge.score);
            }
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
        }
        Ok(())
    })?;
    Ok((links, bytes))
}

fn add_semantic_link(
    links: &mut SemanticLinks,
    bytes: &mut usize,
    source: SymbolId,
    target: SymbolId,
    score: f64,
) {
    if !links.contains_key(&source) {
        *bytes += 128;
    }
    let peers = links.entry(source).or_default();
    if !peers.contains_key(&target) {
        *bytes += 160;
    }
    peers
        .entry(target)
        .and_modify(|old| *old = old.max(score))
        .or_insert(score);
}

fn connected_members(
    links: &SemanticLinks,
    start: &SymbolId,
    visited: &mut BTreeSet<SymbolId>,
    budget: &mut ReadBudget,
) -> Result<Vec<SymbolId>, RocksError> {
    let mut pending = vec![start.clone()];
    let mut members = Vec::new();
    while let Some(id) = pending.pop() {
        budget.checkpoint()?;
        if !visited.insert(id.clone()) {
            continue;
        }
        members.push(id.clone());
        if let Some(peers) = links.get(&id) {
            pending.extend(peers.keys().filter(|id| !visited.contains(*id)).cloned());
        }
        if pending.len() > 65_536 {
            return Err(workspace());
        }
    }
    members.sort();
    Ok(members)
}
fn add_group(
    groups: &mut Groups,
    hash: [u8; 32],
    id: &SymbolId,
    bytes: &mut usize,
) -> Result<(), RocksError> {
    if !groups.contains_key(&hash) {
        *bytes = bytes.checked_add(128).ok_or_else(workspace)?;
    }
    *bytes = bytes.checked_add(96).ok_or_else(workspace)?;
    groups.entry(hash).or_default().push(id.clone());
    Ok(())
}
fn members<'a>(
    ids: impl Iterator<Item = &'a SymbolId>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<Vec<Value>, RocksError> {
    let mut output = Vec::new();
    for id in ids {
        budget.checkpoint()?;
        let symbol = graph.symbol(id, budget)?.ok_or(corrupt())?;
        let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
        let row = (
            file.normalized_path,
            symbol.start_line,
            id.clone(),
            symbol.qualified_name,
        );
        retain(&mut output, row, 10, |a, b| {
            a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
        });
    }
    Ok(output.into_iter().map(|(path, line, id, name)|json!({ "symbolId":id, "qualifiedName":name, "location":format!("{path}:{line}") })).collect())
}
