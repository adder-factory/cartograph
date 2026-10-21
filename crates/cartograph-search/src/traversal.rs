use std::collections::{BTreeMap, BTreeSet};

use cartograph_db::CurrentGraphEdge;
use cartograph_domain::SymbolId;

use crate::{AffectedTest, TraversalDirection, TraversalResult};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GraphArc {
    pub(crate) source: SymbolId,
    pub(crate) target: SymbolId,
    pub(crate) edge_kind: String,
    pub(crate) confidence: f32,
    pub(crate) provenance: String,
    pub(crate) site_count: u32,
}

impl GraphArc {
    pub(crate) fn from_record(edge: &CurrentGraphEdge) -> Self {
        Self {
            source: edge.source_symbol_id().clone(),
            target: edge.target_symbol_id().clone(),
            edge_kind: edge.edge_kind().to_owned(),
            confidence: edge.confidence(),
            provenance: edge.provenance().to_owned(),
            site_count: edge.site_count(),
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture(input: GraphArcFixture<'_>) -> Self {
        Self {
            source: input.source,
            target: input.target,
            edge_kind: input.edge_kind.to_owned(),
            confidence: input.confidence,
            provenance: "fixture".to_owned(),
            site_count: input.site_count,
        }
    }

    pub(crate) fn adjacent(&self, direction: TraversalDirection) -> &SymbolId {
        match direction {
            TraversalDirection::Outgoing => &self.target,
            TraversalDirection::Incoming => &self.source,
        }
    }

    pub(crate) fn origin(&self, direction: TraversalDirection) -> &SymbolId {
        match direction {
            TraversalDirection::Outgoing => &self.source,
            TraversalDirection::Incoming => &self.target,
        }
    }
}

#[cfg(test)]
pub(crate) struct GraphArcFixture<'input> {
    pub(crate) source: SymbolId,
    pub(crate) target: SymbolId,
    pub(crate) edge_kind: &'input str,
    pub(crate) confidence: f32,
    pub(crate) site_count: u32,
}

pub(crate) struct FrontierExpansion {
    pub(crate) next: Vec<SymbolId>,
    pub(crate) arcs: Vec<GraphArc>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct FrontierInput<'input> {
    pub(crate) frontier: &'input [SymbolId],
    pub(crate) arcs: &'input [GraphArc],
    pub(crate) direction: TraversalDirection,
    pub(crate) max_new_nodes: usize,
}

pub(crate) fn expand_frontier(input: FrontierInput<'_>) -> FrontierExpansion {
    let frontier_rank = input
        .frontier
        .iter()
        .enumerate()
        .map(|(rank, symbol_id)| (symbol_id, rank))
        .collect::<BTreeMap<_, _>>();
    let mut ordered_arcs = input
        .arcs
        .iter()
        .filter(|arc| frontier_rank.contains_key(arc.origin(input.direction)))
        .cloned()
        .collect::<Vec<_>>();
    ordered_arcs.sort_by(|left, right| {
        frontier_rank[left.origin(input.direction)]
            .cmp(&frontier_rank[right.origin(input.direction)])
            .then_with(|| strongest_arc_order(left, right))
            .then_with(|| {
                left.adjacent(input.direction)
                    .cmp(right.adjacent(input.direction))
            })
            .then_with(|| left.edge_kind.cmp(&right.edge_kind))
            .then_with(|| left.provenance.cmp(&right.provenance))
    });
    let mut candidates_by_root = input
        .frontier
        .iter()
        .map(|_| Vec::<SymbolId>::new())
        .collect::<Vec<_>>();
    for arc in &ordered_arcs {
        let Some(root_rank) = frontier_rank.get(arc.origin(input.direction)).copied() else {
            continue;
        };
        let adjacent = arc.adjacent(input.direction);
        if !candidates_by_root[root_rank].contains(adjacent) {
            candidates_by_root[root_rank].push(adjacent.clone());
        }
    }
    let total_candidates = candidates_by_root
        .iter()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .len();
    let maximum_fanout = candidates_by_root
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut all_next = Vec::new();
    for offset in 0..maximum_fanout {
        for candidates in &candidates_by_root {
            let Some(adjacent) = candidates.get(offset) else {
                continue;
            };
            if seen.insert(adjacent.clone()) {
                all_next.push(adjacent.clone());
            }
        }
    }
    let truncated = total_candidates > input.max_new_nodes;
    all_next.truncate(input.max_new_nodes);
    FrontierExpansion {
        next: all_next,
        arcs: ordered_arcs,
        truncated,
    }
}

pub(crate) fn strongest_arc<'arcs>(
    arcs: &'arcs [GraphArc],
    symbol_id: &SymbolId,
    direction: TraversalDirection,
) -> Option<&'arcs GraphArc> {
    arcs.iter()
        .filter(|arc| arc.adjacent(direction) == symbol_id)
        .min_by(|left, right| strongest_arc_order(left, right))
}

fn strongest_arc_order(left: &GraphArc, right: &GraphArc) -> std::cmp::Ordering {
    right
        .confidence
        .total_cmp(&left.confidence)
        .then_with(|| right.site_count.cmp(&left.site_count))
        .then_with(|| left.edge_kind.cmp(&right.edge_kind))
        .then_with(|| left.provenance.cmp(&right.provenance))
        .then_with(|| left.source.cmp(&right.source))
        .then_with(|| left.target.cmp(&right.target))
}

/// Return whether a canonical path uses a conventional test directory or filename.
#[must_use]
pub fn is_test_path(path: &str) -> bool {
    cartograph_domain::is_test_source_path(path)
}

pub(crate) fn affected_tests_from_traversal(
    traversal: &TraversalResult,
    limit: u16,
) -> (Vec<AffectedTest>, bool) {
    affected_tests_from_nodes(traversal.nodes(), limit)
}

pub(crate) fn affected_tests_from_nodes(
    nodes: &[crate::TraversalNode],
    limit: u16,
) -> (Vec<AffectedTest>, bool) {
    let mut by_file = BTreeMap::new();
    for node in nodes {
        let edge_is_test = node.via().edge_kind() == "tests";
        if !edge_is_test && !is_test_path(node.symbol().path().as_str()) {
            continue;
        }
        let reason = if edge_is_test {
            "tests-edge"
        } else {
            "test-path"
        };
        by_file
            .entry(node.symbol().file_id().clone())
            .or_insert_with(|| {
                AffectedTest::new(node.symbol().clone(), node.depth(), reason.to_owned())
            });
    }
    let mut tests = by_file.into_values().collect::<Vec<_>>();
    tests.sort_by(|left, right| {
        left.distance()
            .cmp(&right.distance())
            .then_with(|| {
                left.symbol()
                    .path()
                    .as_str()
                    .cmp(right.symbol().path().as_str())
            })
            .then_with(|| left.symbol().start_line().cmp(&right.symbol().start_line()))
            .then_with(|| {
                left.symbol()
                    .qualified_name()
                    .cmp(right.symbol().qualified_name())
            })
            .then_with(|| left.symbol().symbol_id().cmp(right.symbol().symbol_id()))
    });
    let truncated = tests.len() > usize::from(limit);
    tests.truncate(usize::from(limit));
    (tests, truncated)
}
