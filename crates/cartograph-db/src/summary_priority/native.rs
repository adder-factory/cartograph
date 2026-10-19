//! Demand queue updates commit with the same generation fence as artifact writes.

use super::{
    SummaryPriorityEnqueueReport, SummaryPriorityFailure, SummaryPriorityQueueStats,
    validate_tokens,
};
use crate::{
    NativeDatabase, StorageError,
    artifacts::native::summaries::{current_symbol_summary, documentation_lengths, summary_kind},
    native_database::{decode, encode, now, timestamp_order, visit_state},
    retrieval::native::{retain, simple_name, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use cartograph_domain::{GenerationId, ProjectId, SymbolId, Visibility};
use serde::{Deserialize, Serialize};

const NAMESPACE: StateNamespace = StateNamespace::Derived;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) enqueued: String,
    pub(crate) requests: u64,
    pub(crate) attempts: u8,
}

impl NativeDatabase {
    /// Record exact-name demand while preserving the poison-row retry budget.
    /// # Errors
    /// Rejects invalid tokens, capacity exhaustion, or a failed atomic native write.
    pub async fn enqueue_summary_candidates_for_intent(
        &self,
        project: &ProjectId,
        tokens: &[String],
        limit: u16,
    ) -> Result<SummaryPriorityEnqueueReport, StorageError> {
        self.require_project(project)?;
        validate_tokens(tokens)?;
        if limit == 0 || limit > 20 {
            return Err(StorageError::InvalidInput {
                field: "summary_priority_limit",
            });
        }
        self.prune_derived_state_with_request(&Self::request_control(
            std::time::Duration::from_secs(30),
        )?)
        .await?;
        let tokens = tokens
            .iter()
            .map(|token| token.to_lowercase())
            .collect::<std::collections::BTreeSet<_>>();
        self.state(None, 1_048_576, 20, move |transaction, budget| {
            let mut report = SummaryPriorityEnqueueReport {
                candidates: 0,
                enqueued: 0,
                refreshed: 0,
            };
            let Some(current) = transaction.current()? else {
                return Ok(report);
            };
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let candidates = summary_candidates(transaction, &graph, budget, &tokens, limit)?;
            let count = queue_count(transaction, budget)?;
            let mut writes = Vec::new();
            for (_, _, _, id) in candidates {
                report.candidates += 1;
                let existing = read(transaction, &current.generation_id, &id, budget)?;
                let entry = if let Some(mut entry) = existing {
                    report.refreshed += 1;
                    entry.requests = entry.requests.saturating_add(1).min(1_000_000_000);
                    entry.enqueued = now();
                    entry
                } else {
                    report.enqueued += 1;
                    Entry {
                        enqueued: now(),
                        requests: 1,
                        attempts: 0,
                    }
                };
                writes.push((key(&current.generation_id, &id), entry));
            }
            if count.saturating_add(report.enqueued) > 20_000 {
                return Err(RocksError::Limit {
                    resource: "summary_priority_rows",
                });
            }
            drop(graph);
            drop(snapshot);
            for (key, entry) in writes {
                transaction.put(NAMESPACE, &key, encode(&entry)?)?;
            }
            Ok(report)
        })
        .await
    }

    /// Read exact demand queue health for the current graph publication.
    /// # Errors
    /// Rejects project mismatch, corrupt queue values, or native read exhaustion.
    pub async fn summary_priority_queue_stats(
        &self,
        project: &ProjectId,
    ) -> Result<SummaryPriorityQueueStats, StorageError> {
        self.require_project(project)?;
        self.state(None, 0, 0, |transaction, budget| {
            let Some(current) = transaction.current()? else {
                return Ok(empty());
            };
            stats(transaction, &current.generation_id, budget)
        })
        .await
    }

    /// Evict a failing current queue entry after its third recorded attempt.
    /// # Errors
    /// Rejects project mismatch, corrupt counters, or atomic native write failure.
    pub async fn record_summary_priority_failure(
        &self,
        project: &ProjectId,
        id: &SymbolId,
    ) -> Result<SummaryPriorityFailure, StorageError> {
        self.require_project(project)?;
        let id = id.clone();
        self.state(None, 1_048_576, 1, move |transaction, budget| {
            let Some(current) = transaction.current()? else {
                return Ok(SummaryPriorityFailure {
                    present: false,
                    evicted: false,
                });
            };
            let Some(mut entry) = read(transaction, &current.generation_id, &id, budget)? else {
                return Ok(SummaryPriorityFailure {
                    present: false,
                    evicted: false,
                });
            };
            entry.attempts = entry.attempts.checked_add(1).ok_or(corrupt())?;
            let evicted = entry.attempts >= 3;
            let key = key(&current.generation_id, &id);
            if evicted {
                transaction.delete(NAMESPACE, &key)?;
            } else {
                transaction.put(NAMESPACE, &key, encode(&entry)?)?;
            }
            Ok(SummaryPriorityFailure {
                present: true,
                evicted,
            })
        })
        .await
    }
}

pub(crate) fn key(generation: &GenerationId, id: &SymbolId) -> Vec<u8> {
    format!("q/{generation}/{id}").into_bytes()
}
pub(crate) fn read(
    transaction: &StateTransaction<'_>,
    generation: &GenerationId,
    id: &SymbolId,
    budget: &mut ReadBudget,
) -> Result<Option<Entry>, RocksError> {
    let entry: Option<Entry> = transaction
        .get(NAMESPACE, &key(generation, id), budget)?
        .map(|bytes| decode(&bytes))
        .transpose()?;
    if let Some(entry) = &entry {
        validate(entry)?;
    }
    Ok(entry)
}
fn validate(entry: &Entry) -> Result<(), RocksError> {
    if entry.enqueued.parse::<jiff::Timestamp>().is_err()
        || !(1..=1_000_000_000).contains(&entry.requests)
        || entry.attempts >= 3
    {
        return Err(corrupt());
    }
    Ok(())
}
fn empty() -> SummaryPriorityQueueStats {
    SummaryPriorityQueueStats {
        pending: 0,
        oldest_enqueued_at: None,
        newest_enqueued_at: None,
        total_requests: 0,
        attempted: 0,
    }
}
fn stats(
    transaction: &StateTransaction<'_>,
    generation: &GenerationId,
    budget: &mut ReadBudget,
) -> Result<SummaryPriorityQueueStats, RocksError> {
    let mut result = empty();
    visit_state(
        transaction,
        NAMESPACE,
        format!("q/{generation}/").as_bytes(),
        budget,
        |_, entry: Entry, _| {
            validate(&entry)?;
            result.pending += 1;
            result.total_requests = result
                .total_requests
                .checked_add(entry.requests)
                .ok_or(corrupt())?;
            result.attempted += u64::from(entry.attempts > 0);
            if result
                .oldest_enqueued_at
                .as_ref()
                .is_none_or(|oldest| timestamp_order(&entry.enqueued, oldest).is_lt())
            {
                result.oldest_enqueued_at = Some(entry.enqueued.clone());
            }
            if result
                .newest_enqueued_at
                .as_ref()
                .is_none_or(|newest| timestamp_order(&entry.enqueued, newest).is_gt())
            {
                result.newest_enqueued_at = Some(entry.enqueued);
            }
            Ok(())
        },
    )?;
    Ok(result)
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "native-summary-priority",
    }
}

type CandidateRank = (bool, bool, Option<u32>, SymbolId);
fn summary_candidates(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    tokens: &std::collections::BTreeSet<String>,
    limit: u16,
) -> Result<Vec<CandidateRank>, RocksError> {
    let lengths = documentation_lengths(graph, budget)?;
    let mut candidates = Vec::new();
    visit_symbols(graph, budget, |symbol, budget| {
        if !summary_kind(&symbol.symbol_kind)
            || lengths.get(&symbol.symbol_id).copied().unwrap_or(0) >= 20
            || !tokens.contains(&simple_name(&symbol.qualified_name).to_lowercase())
        {
            return Ok(());
        }
        if current_symbol_summary(transaction, &symbol, budget)?.is_some() {
            return Ok(());
        }
        let rank = (
            symbol.export.exported,
            symbol.visibility == Some(Visibility::Public),
            symbol.pagerank_ppb,
            symbol.symbol_id,
        );
        retain(&mut candidates, rank, usize::from(limit), |a, b| {
            b.0.cmp(&a.0)
                .then(b.1.cmp(&a.1))
                .then(b.2.cmp(&a.2))
                .then(a.3.cmp(&b.3))
        });
        Ok(())
    })?;
    Ok(candidates)
}

fn queue_count(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<u64, RocksError> {
    let mut count = 0_u64;
    let mut retained_bytes = 0_usize;
    visit_state::<Entry>(transaction, NAMESPACE, b"q/", budget, |key, entry, _| {
        validate(&entry)?;
        count += 1;
        retained_bytes = retained_bytes
            .checked_add(key.len() + encode(&entry)?.len())
            .ok_or(corrupt())?;
        if count > 20_000 || retained_bytes > 8 * 1_048_576 {
            return Err(RocksError::Limit {
                resource: "summary_priority_retention",
            });
        }
        Ok(())
    })?;
    Ok(count)
}
