//! Issue evidence is published as one generation-fenced typed snapshot.

use super::{
    IssueAttributionKind as Kind, IssueCommitSymbolPeerQuery, IssueHistoryError as Error,
    IssueHistoryRefreshReport, IssueHistoryRefreshRequest, SymbolIssueAttribution as Row,
    SymbolIssueCommitPeer, SymbolIssueCommitPeers, SymbolIssuePeer, SymbolIssuePeerQuery,
    SymbolIssueQuery, SymbolIssueRecord, valid_commit_sha, valid_issue_commit_peer_query,
};
use crate::{
    NativeDatabase, StorageError,
    native_database::dataset,
    retrieval::native::retain,
    rocks::{ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use cartograph_domain::{GenerationId, ProjectId, SymbolId};
use std::collections::{BTreeMap, BTreeSet};

const NAMESPACE: StateNamespace = StateNamespace::History;
const NAME: &str = "issues";

impl NativeDatabase {
    /// Clear issue evidence only while the expected graph is current.
    /// # Errors
    /// Rejects project mismatch, changed publication, or a failed pointer update.
    pub async fn clear_issue_history(
        &self,
        project: &ProjectId,
        generation: &GenerationId,
    ) -> Result<(), Error> {
        self.require_project(project)?;
        self.state(Some(generation.clone()), 4096, 1, |transaction, _| {
            transaction.delete(NAMESPACE, b"dataset/h/issues")
        })
        .await
        .map_err(Error::from)
    }

    /// Prepare issue rows and atomically publish them with their exact scan provenance.
    /// # Errors
    /// Rejects stale generations, malformed rows, or bounded snapshot preparation failures.
    pub async fn replace_issue_history(
        &self,
        request: IssueHistoryRefreshRequest,
    ) -> Result<IssueHistoryRefreshReport, Error> {
        self.require_project(&request.project_id)?;
        let candidates = request.attributions.len() as u64;
        let rows = self
            .graph(
                &request.generation_id,
                super::ISSUE_HISTORY_TIMEOUT,
                move |graph, budget| {
                    let mut rows = Vec::new();
                    let mut seen = BTreeSet::new();
                    let mut workspace = 0_usize;
                    for row in request.attributions {
                        validate(&row)?;
                        if graph.symbol(&row.symbol_id, budget)?.is_none() {
                            continue;
                        }
                        let identity = (
                            row.symbol_id.clone(),
                            row.issue_number,
                            row.commit_sha.clone(),
                            row.kind,
                        );
                        if seen.insert(identity) {
                            workspace = workspace.saturating_add(row.commit_sha.len() * 2 + 384);
                            if workspace > 64 * 1_048_576 {
                                return Err(limit());
                            }
                            rows.push(row);
                        }
                    }
                    Ok(rows)
                },
            )
            .await?;
        let report = IssueHistoryRefreshReport {
            generation_id: request.generation_id.clone(),
            head_commit: request.metadata.head_commit,
            commits_scanned: request.metadata.commits_scanned,
            tagged_commits: request.metadata.tagged_commits,
            oversized_commits_skipped: request.metadata.oversized_commits_skipped,
            comparison_failures_skipped: request.metadata.comparison_failures_skipped,
            candidates,
            attributions_written: rows.len() as u64,
            truncated: request.metadata.truncated,
        };
        self.replace_dataset(
            NAMESPACE,
            NAME,
            Some(request.generation_id),
            &report,
            rows.into_iter(),
        )
        .await?;
        Ok(report)
    }

    /// List exact current attributions for one symbol in issue/commit/kind order.
    /// # Errors
    /// Rejects invalid bounds, corrupt snapshots, or bounded read exhaustion.
    pub async fn current_symbol_issues(
        &self,
        request: SymbolIssueQuery<'_>,
    ) -> Result<Vec<SymbolIssueRecord>, Error> {
        self.require_project(request.project_id)?;
        if request.limit == 0 || request.limit > 500 {
            return Err(Error::InvalidInput);
        }
        let (id, limit) = (request.symbol_id.clone(), usize::from(request.limit));
        self.state(None, 0, 0, move |transaction, budget| {
            let mut rows = Vec::new();
            visit(transaction, budget, |row, _| {
                if row.symbol_id == id {
                    retain(
                        &mut rows,
                        SymbolIssueRecord {
                            issue_number: row.issue_number,
                            commit_sha: row.commit_sha,
                            kind: row.kind,
                        },
                        limit,
                        |a, b| {
                            a.issue_number
                                .cmp(&b.issue_number)
                                .then(a.commit_sha.cmp(&b.commit_sha))
                                .then(a.kind.cmp(&b.kind))
                        },
                    );
                }
                Ok(())
            })?;
            Ok(rows)
        })
        .await
        .map_err(Error::from)
    }

    /// Count complete current issue facts without retaining their payload.
    /// # Errors
    /// Rejects project mismatch, corrupt snapshots, or bounded read exhaustion.
    pub async fn current_issue_history_attribution_count(
        &self,
        project: &ProjectId,
    ) -> Result<u64, Error> {
        self.require_project(project)?;
        self.state(None, 0, 0, |transaction, budget| {
            let mut count = 0_u64;
            visit(transaction, budget, |_, _| {
                count += 1;
                Ok(())
            })?;
            Ok(count)
        })
        .await
        .map_err(Error::from)
    }

    /// Rank peers by distinct modified commits shared with an anchor symbol.
    /// # Errors
    /// Rejects invalid bounds, oversized join workspace, or inconsistent native evidence.
    pub async fn current_symbol_issue_peers(
        &self,
        request: SymbolIssuePeerQuery<'_>,
    ) -> Result<Vec<SymbolIssuePeer>, Error> {
        self.require_project(request.project_id)?;
        if request.minimum_shared == 0 || request.limit == 0 || request.limit > 500 {
            return Err(Error::InvalidInput);
        }
        let (anchor, minimum, maximum) = (
            request.symbol_id.clone(),
            u64::from(request.minimum_shared),
            usize::from(request.limit),
        );
        self.state(None, 0, 0, move |transaction, budget| {
            if transaction.current()?.is_none() {
                return Ok(Vec::new());
            }
            let shared = shared_commits(modified_commits(transaction, budget)?, &anchor)?;
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let mut output = Vec::new();
            for (id, commits) in shared {
                if (commits.len() as u64) < minimum {
                    continue;
                }
                let Some(symbol) = graph.symbol(&id, budget)? else {
                    continue;
                };
                let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                retain(
                    &mut output,
                    SymbolIssuePeer {
                        symbol_id: id,
                        path: file.normalized_path,
                        qualified_name: symbol.qualified_name,
                        co_occurrences: commits.len() as u64,
                        shared_commits: commits.into_iter().take(5).collect(),
                    },
                    maximum,
                    |a, b| {
                        b.co_occurrences
                            .cmp(&a.co_occurrences)
                            .then(a.path.cmp(&b.path))
                            .then(a.qualified_name.cmp(&b.qualified_name))
                            .then(a.symbol_id.cmp(&b.symbol_id))
                    },
                );
            }
            Ok(output)
        })
        .await
        .map_err(Error::from)
    }

    /// Return one exact, bounded current symbol page per requested tagged commit.
    /// # Errors
    /// Rejects invalid commit/bounds, corrupt snapshots, or exhausted bounded reads.
    pub async fn current_issue_commit_symbol_peers(
        &self,
        request: IssueCommitSymbolPeerQuery<'_>,
    ) -> Result<Vec<SymbolIssueCommitPeers>, Error> {
        self.require_project(request.project_id)?;
        if !valid_issue_commit_peer_query(&request) {
            return Err(Error::InvalidInput);
        }
        let (commits, excluded, maximum) = (
            request.commits.to_vec(),
            request.excluded_symbol_id.clone(),
            usize::from(request.per_commit_limit),
        );
        self.state(None, 0, 0, move |transaction, budget| {
            let mut requested = commits
                .iter()
                .cloned()
                .map(|commit| (commit, BTreeSet::new()))
                .collect::<BTreeMap<_, BTreeSet<SymbolId>>>();
            visit(transaction, budget, |row, _| {
                if row.kind == Kind::Modified
                    && row.symbol_id != excluded
                    && let Some(ids) = requested.get_mut(&row.commit_sha)
                {
                    ids.insert(row.symbol_id);
                }
                if requested.values().map(BTreeSet::len).sum::<usize>() > 131_072 {
                    return Err(limit());
                }
                Ok(())
            })?;
            if transaction.current()?.is_none() {
                return Ok(commits
                    .into_iter()
                    .map(|commit_sha| SymbolIssueCommitPeers {
                        commit_sha,
                        total: 0,
                        peers: Vec::new(),
                        truncated: false,
                    })
                    .collect());
            }
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let mut output = Vec::new();
            for commit in commits {
                let mut peers = Vec::new();
                let mut total = 0_u64;
                for id in requested.remove(&commit).unwrap_or_default() {
                    let Some(symbol) = graph.symbol(&id, budget)? else {
                        continue;
                    };
                    let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                    total += 1;
                    retain(
                        &mut peers,
                        SymbolIssueCommitPeer {
                            symbol_id: id,
                            path: file.normalized_path,
                            qualified_name: symbol.qualified_name,
                            symbol_kind: symbol.symbol_kind,
                        },
                        maximum,
                        |a, b| {
                            a.path
                                .cmp(&b.path)
                                .then(a.qualified_name.cmp(&b.qualified_name))
                                .then(a.symbol_id.cmp(&b.symbol_id))
                        },
                    );
                }
                output.push(SymbolIssueCommitPeers {
                    commit_sha: commit,
                    total,
                    truncated: total > peers.len() as u64,
                    peers,
                });
            }
            Ok(output)
        })
        .await
        .map_err(Error::from)
    }
}

fn shared_commits(
    commits: BTreeMap<String, BTreeSet<SymbolId>>,
    anchor: &SymbolId,
) -> Result<BTreeMap<SymbolId, BTreeSet<String>>, RocksError> {
    let mut shared = BTreeMap::<SymbolId, BTreeSet<String>>::new();
    let mut bytes = 0_usize;
    for (commit, symbols) in commits {
        if symbols.len() > 50 || !symbols.contains(anchor) {
            continue;
        }
        for id in symbols {
            if &id == anchor {
                continue;
            }
            if shared.entry(id).or_default().insert(commit.clone()) {
                bytes = bytes.saturating_add(commit.len() + 160);
            }
            if bytes > 32 * 1_048_576 {
                return Err(limit());
            }
        }
    }
    Ok(shared)
}

fn visit(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    mut visitor: impl FnMut(Row, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let current = transaction
        .current()?
        .map(|revision| revision.generation_id);
    let Some(manifest) = dataset::manifest(transaction, NAMESPACE, NAME, budget)? else {
        return Ok(());
    };
    if current.is_none() || manifest.generation != current {
        return Ok(());
    }
    dataset::visit(
        transaction,
        NAMESPACE,
        NAME,
        &manifest,
        budget,
        |row: Row, budget| {
            validate(&row)?;
            visitor(row, budget)
        },
    )
}
fn modified_commits(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<String, BTreeSet<SymbolId>>, RocksError> {
    let mut commits = BTreeMap::<String, BTreeSet<SymbolId>>::new();
    let mut bytes = 0_usize;
    visit(transaction, budget, |row, _| {
        if row.kind == Kind::Modified
            && commits
                .entry(row.commit_sha.clone())
                .or_default()
                .insert(row.symbol_id)
        {
            bytes = bytes.saturating_add(row.commit_sha.len() + 160);
        }
        if bytes > 32 * 1_048_576 {
            return Err(limit());
        }
        Ok(())
    })?;
    Ok(commits)
}
fn validate(row: &Row) -> Result<(), RocksError> {
    if row.issue_number == 0
        || i64::try_from(row.issue_number).is_err()
        || !valid_commit_sha(&row.commit_sha)
    {
        return Err(corrupt());
    }
    Ok(())
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "native-issue-history",
    }
}
fn limit() -> RocksError {
    RocksError::Limit {
        resource: "issue-history-workspace",
    }
}
impl From<StorageError> for Error {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::CurrentGenerationChanged => Self::CurrentGenerationChanged,
            StorageError::InvalidInput { .. } => Self::InvalidInput,
            StorageError::CorruptStoredValue { .. } => Self::CorruptStoredValue,
            _ => Self::DatabaseOperation {
                operation: "native-issue-history",
            },
        }
    }
}
