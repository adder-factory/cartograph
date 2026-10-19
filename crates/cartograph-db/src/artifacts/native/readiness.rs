//! Summary and role counts share one generation and bounded native batch reads.

use super::{
    AgentRoleCount, Kind, NAMESPACE, Record, Scope, State, corrupt, counter, indexed, primary,
    scope_key, summaries, validate_record, workspace,
};
use crate::{
    NativeDatabase, StorageError, SummaryCandidatePolicy, SummaryCoverageStats,
    native_database::decode,
    retrieval::native::visit_symbols,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction},
};
use cartograph_domain::{ContentDigest, GenerationId, ProjectId, SymbolId};
use std::collections::BTreeMap;

const PAGE: usize = 16;

struct Candidate {
    id: SymbolId,
    digest: ContentDigest,
    summary: bool,
}

struct Target<'a> {
    candidate: &'a Candidate,
    kind: Kind,
}

struct Counts {
    summaries: SummaryCoverageStats,
    roles: BTreeMap<String, u64>,
    retained: usize,
}

impl NativeDatabase {
    /// Count digest-compatible roles and policy-eligible summaries in one snapshot.
    /// Scope and record lookups use native batches of at most thirty-two keys.
    /// # Errors
    /// Rejects a changed generation, corrupt scope indexes, or exhausted read bounds.
    pub async fn current_agent_readiness(
        &self,
        project: &ProjectId,
        generation: &GenerationId,
        policy: &SummaryCandidatePolicy,
    ) -> Result<(SummaryCoverageStats, Vec<AgentRoleCount>), StorageError> {
        self.require_project(project)?;
        let policy = policy.clone();
        self.state(Some(generation.clone()), 0, 0, move |tx, budget| {
            let snapshot = tx.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            counts(tx, &graph, &policy, budget)
        })
        .await
    }
}

fn counts(
    tx: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    policy: &SummaryCandidatePolicy,
    budget: &mut ReadBudget,
) -> Result<(SummaryCoverageStats, Vec<AgentRoleCount>), RocksError> {
    let lengths = summaries::documentation_lengths(graph, budget)?;
    let mut output = Counts {
        summaries: SummaryCoverageStats {
            eligible_symbols: 0,
            summarized_symbols: 0,
            pending_symbols: 0,
            by_model: BTreeMap::new(),
        },
        roles: BTreeMap::new(),
        retained: 0,
    };
    let mut page = Vec::with_capacity(PAGE);
    visit_symbols(graph, budget, |symbol, budget| {
        let summary = summaries::eligible(&symbol, policy, &lengths);
        output.summaries.eligible_symbols += u64::from(summary);
        page.push(Candidate {
            id: symbol.symbol_id,
            digest: symbol.structural_digest,
            summary,
        });
        if page.len() == PAGE {
            read_page(tx, &page, &mut output, budget)?;
            page.clear();
        }
        Ok(())
    })?;
    if !page.is_empty() {
        read_page(tx, &page, &mut output, budget)?;
    }
    output.summaries.pending_symbols =
        output.summaries.eligible_symbols - output.summaries.summarized_symbols;
    let mut roles = output
        .roles
        .into_iter()
        .map(|(role, symbols)| AgentRoleCount { role, symbols })
        .collect::<Vec<_>>();
    roles.sort_by(|a, b| b.symbols.cmp(&a.symbols).then(a.role.cmp(&b.role)));
    Ok((output.summaries, roles))
}

fn read_page(
    tx: &StateTransaction<'_>,
    page: &[Candidate],
    output: &mut Counts,
    budget: &mut ReadBudget,
) -> Result<(), RocksError> {
    let mut targets = Vec::with_capacity(PAGE * 2);
    for candidate in page {
        targets.push(Target {
            candidate,
            kind: Kind::Role,
        });
        if candidate.summary {
            targets.push(Target {
                candidate,
                kind: Kind::Summary,
            });
        }
    }
    let keys = targets
        .iter()
        .map(|target| {
            scope_key(target.kind, Scope::Symbol, target.candidate.id.as_str()).into_bytes()
        })
        .collect::<Vec<_>>();
    let pointers = tx.get_many(NAMESPACE, &keys, budget)?;
    let mut selected = Vec::new();
    for (target, pointer) in targets.into_iter().zip(pointers) {
        if let Some(pointer) = pointer {
            selected.push((target, counter(&pointer)?));
        }
    }
    if selected.is_empty() {
        return Ok(());
    }
    let keys = selected
        .iter()
        .map(|(_, id)| primary(*id).into_bytes())
        .collect::<Vec<_>>();
    let rows = tx.get_many(NAMESPACE, &keys, budget)?;
    for ((target, id), bytes) in selected.into_iter().zip(rows) {
        let record: Record = decode(&bytes.ok_or_else(corrupt)?)?;
        validate_record(&record)?;
        if record.id != id
            || record.kind != target.kind
            || record.scope != Scope::Symbol
            || record.scope_key != target.candidate.id.as_str()
            || !indexed(record.kind, record.state)
        {
            return Err(corrupt());
        }
        if record.source_digest.as_deref() == Some(target.candidate.digest.as_str()) {
            count_record(output, record)?;
        }
    }
    Ok(())
}

fn count_record(output: &mut Counts, record: Record) -> Result<(), RocksError> {
    if record.kind == Kind::Role && matches!(record.state, State::Active | State::Complete) {
        count(&mut output.roles, record.body, &mut output.retained)?;
    } else if record.kind == Kind::Summary && record.state == State::Complete {
        let model = record
            .metadata
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown")
            .to_owned();
        count(&mut output.summaries.by_model, model, &mut output.retained)?;
        output.summaries.summarized_symbols += 1;
    }
    Ok(())
}

fn count(
    map: &mut BTreeMap<String, u64>,
    key: String,
    retained: &mut usize,
) -> Result<(), RocksError> {
    if !map.contains_key(&key) {
        *retained = retained.saturating_add(key.len() + 128);
        if map.len() >= 8192 || *retained > 32 * 1_048_576 {
            return Err(workspace());
        }
    }
    *map.entry(key).or_default() += 1;
    Ok(())
}

#[cfg(test)]
mod tests;
