//! Navigation bookkeeping, kept apart from the fenced I/O in `Navigator`.
//!
//! The ledger records the evidence one navigation has observed and the
//! operation budget it has spent. A round prompt is the provider's bounded
//! view of that ledger for one round, and a round outcome is the provider's
//! answer read back into a stop decision or a plan of operations. Nothing here
//! reads the database or calls the provider.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_db::CurrentSymbolRecord;
use cartograph_domain::SymbolId;
use cartograph_llm::{JevAnswer, JevDecision, JevQuestion};
use cartograph_search::TraversalResult;
use serde_json::{Value, json};

use super::{
    Action, AvailableActions, CANDIDATE_TEXT_LIMIT, INITIAL_CANDIDATE_LIMIT,
    LOOKUP_CANDIDATE_LIMIT, MAXIMUM_NATIVE_SOURCE_BYTES, MAXIMUM_READS_PER_ROUND,
    MAXIMUM_RELEVANCE_QUESTIONS, MAXIMUM_STEPS, NavigationCandidate, NavigationReport,
    NavigationRequest, NavigationStep, NavigationStop, RELEVANCE_READ, STATE_BUDGET_BYTES,
    SUFFICIENCY_STOP, action_key, admit_candidate, admit_record, bounded_signature,
    query_identifiers, questions, relevance_key, selected_step,
};
use crate::{ProjectError, SymbolSourceContext};

/// What one navigation has observed and spent: the report returned to the
/// caller, the operations that may not be offered again, the operation count,
/// and the caller-supplied source admitted as evidence.
pub(super) struct NavigationLedger<'a> {
    /// The user's question, repeated in every round's provider state.
    task: &'a str,
    /// Evidence and decisions returned to the caller.
    pub(super) report: NavigationReport,
    /// Keys of operations executed, or covered by complete native source;
    /// none of them is offered again.
    pub(super) used: BTreeSet<String>,
    /// Retrieval operations spent against [`MAXIMUM_STEPS`].
    pub(super) operations: usize,
    /// Caller-supplied source windows admitted within the byte budget.
    native_sources: Vec<&'a SymbolSourceContext>,
}

impl<'a> NavigationLedger<'a> {
    /// Seed candidates from the request's packet evidence and admit its
    /// native source windows. A complete native window counts as already read.
    pub(super) fn new(request: &NavigationRequest<'a>) -> Self {
        let mut report = NavigationReport::new(
            NavigationStop::NotConfigured,
            request
                .packet
                .generation()
                .map(|g| g.generation_id().clone()),
        );
        for evidence in request.packet.evidence() {
            let Some(id) = evidence.symbol_id() else {
                continue;
            };
            if report
                .candidates
                .iter()
                .any(|candidate| &candidate.symbol_id == id)
            {
                continue;
            }
            if report.candidates.len() >= INITIAL_CANDIDATE_LIMIT {
                if evidence.path().len() <= CANDIDATE_TEXT_LIMIT
                    && evidence.qualified_name().len() <= CANDIDATE_TEXT_LIMIT
                {
                    report.candidates_truncated = true;
                }
                continue;
            }
            admit_candidate(
                &mut report,
                NavigationCandidate {
                    symbol_id: id.clone(),
                    path: evidence.path().to_owned(),
                    name: evidence.qualified_name().to_owned(),
                    symbol_kind: String::new(),
                    signature: String::new(),
                    start_line: evidence.start_line(),
                    end_line: evidence.end_line(),
                    relevance: None,
                },
            );
        }
        let mut native_sources = Vec::new();
        let mut native_bytes = 0_usize;
        let mut used = BTreeSet::new();
        for source in request.native_sources {
            let Some(excerpt) = source.excerpt() else {
                continue;
            };
            let size = serde_json::to_vec(source).map_or(usize::MAX, |bytes| bytes.len());
            if native_bytes.saturating_add(size) > MAXIMUM_NATIVE_SOURCE_BYTES {
                report.native_sources_truncated = true;
                continue;
            }
            native_bytes += size;
            native_sources.push(source);
            if !excerpt.truncated() {
                used.insert(action_key(&Action::Read {
                    symbol: source.symbol().symbol_id().clone(),
                }));
            }
        }
        report.native_source_windows = native_sources.len();
        Self {
            task: request.task,
            report,
            used,
            operations: 0,
            native_sources,
        }
    }

    /// Fill seeded candidates with their indexed kind, signature and lines,
    /// dropping any candidate the generation's `records` no longer hold.
    pub(super) fn hydrate(&mut self, records: &[CurrentSymbolRecord]) {
        let original_count = self.report.candidates.len();
        self.report.candidates.retain_mut(|candidate| {
            let Some(record) = records
                .iter()
                .find(|record| record.symbol_id() == &candidate.symbol_id)
            else {
                return false;
            };
            candidate.symbol_kind = record.symbol_kind().to_owned();
            candidate.signature = bounded_signature(record.signature());
            candidate.start_line = Some(record.start_line());
            candidate.end_line = Some(record.end_line());
            true
        });
        self.report.candidates_truncated |= self.report.candidates.len() < original_count;
    }

    /// Spend one operation per source read; none of them is offered again.
    pub(super) fn record_reads(&mut self, symbols: &[SymbolId]) {
        for symbol in symbols {
            self.used.insert(action_key(&Action::Read {
                symbol: symbol.clone(),
            }));
        }
        self.operations += symbols.len();
    }

    /// Spend one operation on `action`; it is not offered again.
    pub(super) fn record_lookup(&mut self, action: &Action) {
        self.used.insert(action_key(action));
        self.operations += 1;
    }

    /// Attach a callers or callees traversal to the step that requested it and
    /// return the symbols it reached.
    pub(super) fn record_graph(&mut self, graph: TraversalResult) -> Vec<CurrentSymbolRecord> {
        self.report.candidates_truncated |= graph.truncated();
        let symbols = graph
            .nodes()
            .iter()
            .map(|node| node.symbol().clone())
            .collect();
        if let Some(step) = self.report.decisions.last_mut() {
            step.graph = Some(graph);
        }
        symbols
    }

    /// Admit a lookup's records as candidates within the candidate bounds.
    /// # Errors
    /// Rejects records from any generation other than the navigated one.
    pub(super) fn admit_lookup(
        &mut self,
        records: &[CurrentSymbolRecord],
    ) -> Result<(), ProjectError> {
        if records
            .iter()
            .any(|record| Some(record.generation_id()) != self.report.generation_id.as_ref())
        {
            return Err(ProjectError::SourceContextUnavailable);
        }
        self.report.candidates_truncated |= records.len() > usize::from(LOOKUP_CANDIDATE_LIMIT);
        for record in records.iter().take(usize::from(LOOKUP_CANDIDATE_LIMIT)) {
            admit_record(&mut self.report, record);
        }
        Ok(())
    }

    /// Mark the steps planned for `round` completed once all of them executed.
    pub(super) fn complete_round(&mut self, round: usize) {
        for step in self.report.decisions.iter_mut().rev() {
            if step.round != round || step.completed {
                break;
            }
            step.completed = true;
        }
    }

    /// Keep the advisory relevance each judged candidate received.
    pub(super) fn record_relevance(&mut self, outcome: &RoundOutcome) {
        for (index, probability) in &outcome.relevance {
            if let Some(candidate) = self.report.candidates.get_mut(*index) {
                candidate.relevance = Some(*probability);
            }
        }
    }

    /// End navigation with `stop`, recording `last` as the step that ended it.
    pub(super) fn finish(&mut self, mut last: NavigationStep, stop: NavigationStop) {
        if stop == NavigationStop::Finished && !matches!(last.action, Action::Finish) {
            // A sufficiency stop does not execute the chosen operation;
            // record the finish that actually ended navigation instead.
            last.action = Action::Finish;
            last.completed = true;
            last.confidence = last.source_sufficiency;
        }
        self.report.decisions.push(last);
        self.report.stop = stop;
    }

    /// Whether any source text, captured or caller-supplied, is in evidence.
    pub(super) fn has_source_evidence(&self) -> bool {
        !self.report.source_windows.is_empty() || !self.native_sources.is_empty()
    }

    /// Operations left in the navigation budget.
    pub(super) const fn remaining_operations(&self) -> usize {
        MAXIMUM_STEPS.saturating_sub(self.operations)
    }
}

/// Optional provider-state detail, dropped in [`STATE_DETAIL`] order while the
/// state exceeds [`STATE_BUDGET_BYTES`].
#[derive(Clone, Copy)]
struct StateDetail {
    /// Whether candidate signatures are included.
    signatures: bool,
    /// Most recent captured source windows included.
    windows: usize,
}

/// The least detail a round sends, even when it still exceeds the budget.
const MINIMAL_STATE: StateDetail = StateDetail {
    signatures: false,
    windows: 0,
};

/// Signatures are the first optional detail dropped, then older source
/// windows, so a large candidate list degrades evidence detail instead of the
/// whole round.
const STATE_DETAIL: [StateDetail; 4] = [
    StateDetail {
        signatures: true,
        windows: usize::MAX,
    },
    StateDetail {
        signatures: false,
        windows: usize::MAX,
    },
    StateDetail {
        signatures: false,
        windows: 2,
    },
    MINIMAL_STATE,
];

/// One round's provider request: the operations offered, the unread
/// candidates whose relevance is judged, and the bounded state both refer to.
pub(super) struct RoundPrompt {
    round: usize,
    actions: BTreeMap<String, (Action, String)>,
    relevance: Vec<usize>,
    /// Provider state within the request bound.
    pub(super) state: Value,
    /// The next-operation and sufficiency questions plus one relevance
    /// question per judged candidate, answered in one request.
    pub(super) questions: BTreeMap<String, JevQuestion>,
}

impl RoundPrompt {
    /// Build round `round`'s request from what `ledger` has observed.
    pub(super) fn new(ledger: &NavigationLedger<'_>, round: usize) -> Self {
        let actions = Self::offered_actions(ledger);
        let relevance = Self::relevance_targets(ledger);
        let state = Self::bounded_state(ledger, round);
        let questions = questions(&actions, &relevance);
        Self {
            round,
            actions,
            relevance,
            state,
            questions,
        }
    }

    /// Read `decision` back against this round's offer. `None` means the next
    /// or sufficiency answer is missing or malformed, or names an operation
    /// that was not offered.
    pub(super) fn outcome(&self, decision: &JevDecision) -> Option<RoundOutcome> {
        let next = selected_step(&self.actions, decision, self.round)?;
        let relevance = self
            .relevance
            .iter()
            .filter_map(|index| match decision.answers.get(&relevance_key(*index)) {
                Some(JevAnswer::Noul { noul }) => Some((*index, *noul)),
                _ => None,
            })
            .collect();
        Some(RoundOutcome { next, relevance })
    }

    /// Finish, abstain, and every unused read, traversal and outline over the
    /// candidates plus exact-name lookups for identifiers copied from the task.
    fn offered_actions(ledger: &NavigationLedger<'_>) -> BTreeMap<String, (Action, String)> {
        let mut actions = AvailableActions {
            used: &ledger.used,
            offered: BTreeMap::from([
            ("finish".to_owned(), (Action::Finish, "Return the captured source evidence when it supports answering the task.".to_owned())),
            ("abstain".to_owned(), (Action::Abstain, "No available operation will find the needed evidence; return native and captured evidence with abstention.".to_owned())),
        ]),
        };
        for (index, candidate) in ledger.report.candidates.iter().enumerate() {
            for (kind, action) in [
                (
                    "read",
                    Action::Read {
                        symbol: candidate.symbol_id.clone(),
                    },
                ),
                (
                    "callers",
                    Action::Callers {
                        symbol: candidate.symbol_id.clone(),
                    },
                ),
                (
                    "callees",
                    Action::Callees {
                        symbol: candidate.symbol_id.clone(),
                    },
                ),
            ] {
                if kind != "read"
                    && !matches!(candidate.symbol_kind.as_str(), "function" | "method")
                {
                    continue;
                }
                actions.add(
                    format!("{kind}_{index}"),
                    action,
                    format!(
                        "{kind} candidate {index}: {} ({}) in {}",
                        candidate.name, candidate.symbol_kind, candidate.path
                    ),
                );
            }
            actions.add(
                format!("outline_{index}"),
                Action::Outline {
                    path: candidate.path.clone(),
                },
                format!("List declarations in {}", candidate.path),
            );
        }
        for (index, name) in query_identifiers(ledger.task).into_iter().enumerate() {
            actions.add(
                format!("exact_{index}"),
                Action::ExactName { name: name.clone() },
                format!("Find exact declaration named {name}, copied from the user's task"),
            );
        }
        actions.offered
    }

    /// Unread candidates, in evidence order, whose relevance is judged this round.
    fn relevance_targets(ledger: &NavigationLedger<'_>) -> Vec<usize> {
        ledger
            .report
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                !ledger.used.contains(&action_key(&Action::Read {
                    symbol: candidate.symbol_id.clone(),
                }))
            })
            .map(|(index, _)| index)
            .take(MAXIMUM_RELEVANCE_QUESTIONS)
            .collect()
    }

    /// Provider state within the request bound, with the most detail that fits.
    fn bounded_state(ledger: &NavigationLedger<'_>, round: usize) -> Value {
        for detail in STATE_DETAIL {
            let state = Self::state_with(ledger, round, detail);
            if serde_json::to_vec(&state).map_or(usize::MAX, |bytes| bytes.len())
                <= STATE_BUDGET_BYTES
            {
                return state;
            }
        }
        Self::state_with(ledger, round, MINIMAL_STATE)
    }

    fn state_with(ledger: &NavigationLedger<'_>, round: usize, detail: StateDetail) -> Value {
        let report = &ledger.report;
        let previous = report
            .decisions
            .iter()
            .map(|step| json!({"action": step.action, "completed": step.completed}))
            .collect::<Vec<_>>();
        let candidates = report
            .candidates
            .iter()
            .map(|candidate| {
                json!({
                    "symbolId": candidate.symbol_id,
                    "path": candidate.path,
                    "name": candidate.name,
                    "symbolKind": candidate.symbol_kind,
                    "signature": if detail.signatures { candidate.signature.as_str() } else { "" },
                    "startLine": candidate.start_line,
                    "endLine": candidate.end_line,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "task": ledger.task,
            "round": round,
            "candidates": candidates,
            "nativeSourceWindows": ledger.native_sources,
            "nativeSourcesTruncated": report.native_sources_truncated,
            "sourceWindows": report.source_windows.iter().rev().take(detail.windows).collect::<Vec<_>>(),
            "sourceWindowsOmitted": report.source_windows.len().saturating_sub(detail.windows),
            "previousActions": previous,
            "candidateListTruncated": report.candidates_truncated,
        })
    }
}

/// One provider round read back: the chosen next operation plus advisory
/// relevance for each judged candidate index.
pub(super) struct RoundOutcome {
    /// The chosen operation with its confidence and the judged sufficiency.
    pub(super) next: NavigationStep,
    relevance: Vec<(usize, f64)>,
}

impl RoundOutcome {
    /// The stop this round ends navigation with, if any. `evidence` says
    /// whether any source text is in evidence: sufficiency stops only with
    /// it, a candidate relevant enough to read keeps navigating, and a finish
    /// without it is an abstention.
    pub(super) fn terminal_stop(&self, evidence: bool) -> Option<NavigationStop> {
        if self.next.source_sufficiency >= SUFFICIENCY_STOP && evidence {
            return Some(NavigationStop::Finished);
        }
        if self
            .relevance
            .iter()
            .any(|(_, probability)| *probability >= RELEVANCE_READ)
        {
            return None;
        }
        match self.next.action {
            Action::Finish if evidence => Some(NavigationStop::Finished),
            Action::Finish | Action::Abstain => Some(NavigationStop::Abstained),
            _ => None,
        }
    }

    /// Relevant unread `candidates` first, then the provider's chosen
    /// operation, all within `budget` remaining operations.
    pub(super) fn plan(
        self,
        candidates: &[NavigationCandidate],
        budget: usize,
    ) -> Vec<NavigationStep> {
        let mut relevant = self
            .relevance
            .iter()
            .filter(|(_, probability)| *probability >= RELEVANCE_READ)
            .copied()
            .collect::<Vec<_>>();
        relevant.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        let mut planned = relevant
            .into_iter()
            .filter_map(|(index, probability)| {
                candidates.get(index).map(|candidate| NavigationStep {
                    action: Action::Read {
                        symbol: candidate.symbol_id.clone(),
                    },
                    confidence: probability,
                    source_sufficiency: self.next.source_sufficiency,
                    completed: false,
                    round: self.next.round,
                    graph: None,
                })
            })
            .take(MAXIMUM_READS_PER_ROUND.min(budget))
            .collect::<Vec<_>>();
        let chosen = action_key(&self.next.action);
        if !matches!(self.next.action, Action::Finish | Action::Abstain)
            && planned.len() < budget
            && planned
                .iter()
                .all(|step| action_key(&step.action) != chosen)
        {
            planned.push(self.next);
        }
        planned
    }
}
