use std::{cmp::Ordering, collections::BTreeMap, collections::BTreeSet};

use crate::{
    AffectedTest, ContextAbstention, ContextGraphDirection, ContextPacket, EditCandidate,
    EditCandidateBasis, EditCandidateSet, EvidenceItem, EvidenceReason, GenerationEvidence,
    HybridSearchPacket, IndexFreshness, RetrievalConfidence, TaskIntent, WorkingTreeOverlay,
    model::{
        ContextPacketDetails, EditCandidateInput, evidence_key, evidence_priority,
        evidence_start_line, merge_evidence,
    },
    traversal::is_test_path,
};

const MAX_EDIT_CANDIDATES: usize = 16;
const MAX_EDIT_CANDIDATE_NAMES: usize = 5;

pub(crate) struct PacketAssembly<'task> {
    pub(crate) task: &'task str,
    pub(crate) generation: Option<GenerationEvidence>,
    pub(crate) intent: TaskIntent,
    pub(crate) graph_direction: Option<ContextGraphDirection>,
    pub(crate) freshness: IndexFreshness,
    pub(crate) evidence: Vec<EvidenceItem>,
    pub(crate) retrieval: HybridSearchPacket,
    pub(crate) affected_tests: Vec<AffectedTest>,
    pub(crate) evidence_limit: u16,
    pub(crate) truncated: bool,
}

pub(crate) fn assemble_packet(input: PacketAssembly<'_>) -> ContextPacket {
    let (evidence, evidence_was_truncated) = bound_evidence(input.evidence, input.evidence_limit);
    let edit_candidates = build_edit_candidates(input.task, &evidence);
    let (confidence, abstention) = classify_packet(
        input.generation.as_ref(),
        input.freshness,
        evidence.as_slice(),
    );
    ContextPacket {
        details: ContextPacketDetails {
            generation: input.generation,
            intent: input.intent,
            graph_direction: input.graph_direction,
            freshness: input.freshness,
            confidence,
            abstention,
            retrieval: input.retrieval,
            evidence,
            edit_candidates,
            affected_tests: input.affected_tests,
            working_tree_overlay: WorkingTreeOverlay::not_checked(),
            truncated: input.truncated || evidence_was_truncated,
            decision_rank: None,
        },
    }
}

#[derive(Default)]
struct EditCandidateAccumulator {
    matched_term_count: u16,
    best_rank: Option<u16>,
    qualified_names: BTreeSet<String>,
}

fn build_edit_candidates(task: &str, evidence: &[EvidenceItem]) -> EditCandidateSet {
    let task_terms = normalized_terms(task);
    let exact = collect_exact_edit_candidates(evidence, &task_terms);
    if !exact.is_empty() {
        return finish_edit_candidates(exact, EditCandidateBasis::ExactAnchor, false);
    }

    let mut candidates = BTreeMap::<String, EditCandidateAccumulator>::new();
    for item in evidence.iter().filter(|item| is_edit_evidence(item)) {
        let Some(rank) = item.fused_rank() else {
            continue;
        };
        let matched = matched_term_count(&task_terms, item);
        if matched == 0 {
            continue;
        }
        let entry = candidates.entry(item.path().to_owned()).or_default();
        match matched.cmp(&entry.matched_term_count) {
            Ordering::Greater => {
                entry.matched_term_count = matched;
                entry.best_rank = Some(rank);
                entry.qualified_names.clear();
            }
            Ordering::Equal => {
                entry.best_rank = minimum_rank(entry.best_rank, Some(rank));
            }
            Ordering::Less => continue,
        }
        entry
            .qualified_names
            .insert(item.qualified_name().to_owned());
    }
    let strongest = candidates
        .values()
        .map(|candidate| candidate.matched_term_count)
        .max()
        .unwrap_or(0);
    if strongest == 0 {
        return EditCandidateSet::default();
    }
    candidates.retain(|_, candidate| candidate.matched_term_count == strongest);
    finish_edit_candidates(candidates, EditCandidateBasis::TaskTerms, false)
}

/// Primary edit files derived from decision-ranked evidence: the distinct files
/// of judged items with relevance at least 0.5, in ranked order. Exact-anchor
/// candidates are authoritative and never replaced; `None` keeps the
/// term-based candidates when no judged item is relevant enough.
pub(crate) fn decision_edit_candidates(
    task: &str,
    evidence: &[EvidenceItem],
    current: &EditCandidateSet,
) -> Option<EditCandidateSet> {
    const RELEVANT: f64 = 0.5;
    if current
        .candidates()
        .iter()
        .any(|candidate| candidate.basis() == EditCandidateBasis::ExactAnchor)
    {
        return None;
    }
    let task_terms = normalized_terms(task);
    let mut order = Vec::<String>::new();
    let mut files = BTreeMap::<String, EditCandidateAccumulator>::new();
    for item in evidence.iter().filter(|item| {
        is_edit_evidence(item)
            && item
                .decision_relevance()
                .is_some_and(|value| value >= RELEVANT)
    }) {
        let path = item.path().to_owned();
        if !files.contains_key(&path) {
            order.push(path.clone());
        }
        let entry = files.entry(path).or_default();
        entry.matched_term_count = entry
            .matched_term_count
            .max(matched_term_count(&task_terms, item));
        entry.best_rank = minimum_rank(entry.best_rank, item.fused_rank());
        entry
            .qualified_names
            .insert(item.qualified_name().to_owned());
    }
    if order.is_empty() {
        return None;
    }
    let truncated = order.len() > MAX_EDIT_CANDIDATES;
    let candidates = order
        .into_iter()
        .take(MAX_EDIT_CANDIDATES)
        .filter_map(|path| {
            let candidate = files.remove(&path)?;
            Some(EditCandidate::new(EditCandidateInput {
                path,
                basis: EditCandidateBasis::DecisionRelevance,
                matched_term_count: candidate.matched_term_count,
                best_rank: candidate.best_rank,
                qualified_names: candidate
                    .qualified_names
                    .into_iter()
                    .take(MAX_EDIT_CANDIDATE_NAMES)
                    .collect(),
            }))
        })
        .collect();
    Some(EditCandidateSet::new(candidates, truncated))
}

fn collect_exact_edit_candidates(
    evidence: &[EvidenceItem],
    task_terms: &BTreeSet<String>,
) -> BTreeMap<String, EditCandidateAccumulator> {
    let mut candidates = BTreeMap::<String, EditCandidateAccumulator>::new();
    for item in evidence.iter().filter(|item| {
        is_edit_evidence(item)
            && item.reasons().iter().any(|reason| {
                matches!(
                    reason,
                    EvidenceReason::ExactName
                        | EvidenceReason::ExactPath
                        | EvidenceReason::ExactReference
                        | EvidenceReason::CoarseReference
                )
            })
    }) {
        let entry = candidates.entry(item.path().to_owned()).or_default();
        entry.matched_term_count = entry
            .matched_term_count
            .max(matched_term_count(task_terms, item));
        entry.best_rank = minimum_rank(entry.best_rank, item.fused_rank());
        entry
            .qualified_names
            .insert(item.qualified_name().to_owned());
    }
    candidates
}

fn finish_edit_candidates(
    candidates: BTreeMap<String, EditCandidateAccumulator>,
    basis: EditCandidateBasis,
    inherited_truncation: bool,
) -> EditCandidateSet {
    let mut candidates = candidates
        .into_iter()
        .map(|(path, candidate)| {
            EditCandidate::new(EditCandidateInput {
                path,
                basis,
                matched_term_count: candidate.matched_term_count,
                best_rank: candidate.best_rank,
                qualified_names: candidate
                    .qualified_names
                    .into_iter()
                    .take(MAX_EDIT_CANDIDATE_NAMES)
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .matched_term_count()
            .cmp(&left.matched_term_count())
            .then_with(|| option_rank_order(left.best_rank(), right.best_rank()))
            .then_with(|| left.path().cmp(right.path()))
    });
    let truncated = inherited_truncation || candidates.len() > MAX_EDIT_CANDIDATES;
    candidates.truncate(MAX_EDIT_CANDIDATES);
    EditCandidateSet::new(candidates, truncated)
}

fn is_edit_evidence(item: &EvidenceItem) -> bool {
    item.symbol_id().is_some() && !item.qualified_name().is_empty() && !is_test_path(item.path())
}

fn matched_term_count(task_terms: &BTreeSet<String>, item: &EvidenceItem) -> u16 {
    let mut candidate_terms = normalized_terms(item.qualified_name());
    candidate_terms.extend(normalized_terms(item.path()));
    u16::try_from(task_terms.intersection(&candidate_terms).count()).unwrap_or(u16::MAX)
}

fn normalized_terms(value: &str) -> BTreeSet<String> {
    split_code_words(value)
        .into_iter()
        .map(|term| canonical_term(&term))
        .filter(|term| term.len() > 1 && !is_stop_term(term))
        .collect()
}

fn split_code_words(value: &str) -> Vec<String> {
    let characters = value.chars().collect::<Vec<_>>();
    let mut words = Vec::new();
    let mut current = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if !character.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        let previous = index
            .checked_sub(1)
            .and_then(|offset| characters.get(offset));
        let next = characters.get(index.saturating_add(1));
        let uppercase_boundary = character.is_uppercase()
            && !current.is_empty()
            && (previous.is_some_and(|value| value.is_lowercase())
                || (previous.is_some_and(|value| value.is_uppercase())
                    && next.is_some_and(|value| value.is_lowercase())));
        if uppercase_boundary {
            words.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn canonical_term(term: &str) -> String {
    match term.to_ascii_lowercase().as_str() {
        "postgre" | "postgresql" => "postgres".to_owned(),
        value => value.to_owned(),
    }
}

fn is_stop_term(term: &str) -> bool {
    matches!(
        term,
        "a" | "after"
            | "an"
            | "and"
            | "before"
            | "change"
            | "code"
            | "creating"
            | "edit"
            | "file"
            | "fix"
            | "from"
            | "in"
            | "is"
            | "makes"
            | "modify"
            | "no"
            | "of"
            | "the"
            | "to"
            | "when"
            | "with"
            | "without"
    )
}

fn minimum_rank(left: Option<u16>, right: Option<u16>) -> Option<u16> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

pub(crate) fn bound_evidence(
    evidence: Vec<EvidenceItem>,
    evidence_limit: u16,
) -> (Vec<EvidenceItem>, bool) {
    let mut positions = BTreeMap::<String, usize>::new();
    let mut merged = Vec::<EvidenceItem>::new();
    for item in evidence {
        let key = evidence_key(&item);
        if let Some(index) = positions.get(&key).copied() {
            merge_evidence(&mut merged[index], item);
        } else {
            positions.insert(key, merged.len());
            merged.push(item);
        }
    }
    merged.sort_by(evidence_order);
    let evidence_was_truncated = merged.len() > usize::from(evidence_limit);
    merged.truncate(usize::from(evidence_limit));
    (merged, evidence_was_truncated)
}

fn evidence_order(left: &EvidenceItem, right: &EvidenceItem) -> std::cmp::Ordering {
    let priority = evidence_priority(left).cmp(&evidence_priority(right));
    if priority != std::cmp::Ordering::Equal {
        return priority;
    }
    if has_exact_anchor(left) && has_exact_anchor(right) {
        return std::cmp::Ordering::Equal;
    }
    if !has_exact_anchor(left) && !has_exact_anchor(right) {
        let fused = option_rank_order(left.fused_rank(), right.fused_rank());
        if fused != std::cmp::Ordering::Equal {
            return fused;
        }
        let graph = graph_evidence_order(left, right);
        if graph != std::cmp::Ordering::Equal {
            return graph;
        }
    }
    left.path()
        .cmp(right.path())
        .then_with(|| evidence_start_line(left).cmp(&evidence_start_line(right)))
        .then_with(|| left.qualified_name().cmp(right.qualified_name()))
        .then_with(|| evidence_key(left).cmp(&evidence_key(right)))
}

fn has_exact_anchor(item: &EvidenceItem) -> bool {
    item.reasons().iter().any(|reason| {
        matches!(
            reason,
            EvidenceReason::ExactName
                | EvidenceReason::ExactPath
                | EvidenceReason::ExactReference
                | EvidenceReason::CoarseReference
        )
    })
}

fn option_rank_order(left: Option<u16>, right: Option<u16>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn graph_evidence_order(left: &EvidenceItem, right: &EvidenceItem) -> std::cmp::Ordering {
    match (left.graph(), right.graph()) {
        (Some(left), Some(right)) => left
            .depth()
            .cmp(&right.depth())
            .then_with(|| right.confidence().total_cmp(&left.confidence()))
            .then_with(|| right.site_count().cmp(&left.site_count())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn classify_packet(
    generation: Option<&GenerationEvidence>,
    freshness: IndexFreshness,
    evidence: &[EvidenceItem],
) -> (RetrievalConfidence, Option<ContextAbstention>) {
    if generation.is_none() {
        return (
            RetrievalConfidence::None,
            Some(ContextAbstention::NoCurrentGeneration),
        );
    }
    if evidence.is_empty() {
        return (
            RetrievalConfidence::None,
            Some(ContextAbstention::NoRelevantEvidence),
        );
    }
    match freshness {
        IndexFreshness::Stale => (
            RetrievalConfidence::Low,
            Some(ContextAbstention::StaleIndex),
        ),
        IndexFreshness::Unknown => (
            RetrievalConfidence::Low,
            Some(ContextAbstention::UnknownFreshness),
        ),
        IndexFreshness::Current => {
            let has_exact = evidence.iter().any(|item| {
                item.reasons().iter().any(|reason| {
                    matches!(
                        reason,
                        EvidenceReason::ExactName
                            | EvidenceReason::ExactPath
                            | EvidenceReason::ExactReference
                    )
                })
            });
            if has_exact {
                (RetrievalConfidence::High, None)
            } else {
                (RetrievalConfidence::Medium, None)
            }
        }
    }
}
