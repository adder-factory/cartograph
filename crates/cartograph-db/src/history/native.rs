//! Durable churn and co-change snapshots with atomic native publication.

use super::{
    FileCochangeFact, FileCochangeQuery, FileCochangeRecord, FileHistoryFact, FileHistoryQuery,
    FileHistoryRecord, HistoryRefreshParameters, HistoryRefreshReport, HistoryRefreshRequest,
    HistoryReuseQuery, validate_query_limit,
};
use crate::{
    NativeDatabase, StorageError,
    native_database::dataset,
    retrieval::native::{retain, visit_files},
    rocks::{ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use cartograph_domain::ProjectId;
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const NAMESPACE: StateNamespace = StateNamespace::History;
const NAME: &str = "git";

#[derive(Serialize, Deserialize)]
enum HistoryRow {
    File(FileHistoryFact),
    Cochange(FileCochangeFact),
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    report: HistoryRefreshReport,
    parameters: Option<HistoryRefreshParameters>,
}

impl NativeDatabase {
    /// Publish one complete Git history snapshot after bounded page preparation.
    /// # Errors
    /// Rejects project mismatch, concurrent replacement, or invalid native storage.
    pub async fn replace_file_history(
        &self,
        request: HistoryRefreshRequest,
    ) -> Result<HistoryRefreshReport, StorageError> {
        self.require_project(&request.project_id)?;
        let report = HistoryRefreshReport {
            head_commit: request.head_commit,
            shallow_history: request.shallow_history,
            commits_scanned: request.commits_scanned,
            truncated: request.truncated,
            oversized_commits_skipped: request.oversized_commits_skipped,
            files_written: request.files.len() as u64,
            cochanges_written: request.cochanges.len() as u64,
            reused: false,
        };
        let metadata = Metadata {
            report: report.clone(),
            parameters: request.parameters,
        };
        self.replace_dataset(
            NAMESPACE,
            NAME,
            None,
            &metadata,
            request
                .files
                .into_iter()
                .map(HistoryRow::File)
                .chain(request.cochanges.into_iter().map(HistoryRow::Cochange)),
        )
        .await?;
        Ok(report)
    }

    /// Reuse history only for this checkout and the exact HEAD/mining contract.
    /// # Errors
    /// Returns corrupt metadata or bounded native read failures.
    pub async fn reusable_history_refresh(
        &self,
        reuse: HistoryReuseQuery<'_>,
    ) -> Result<Option<HistoryRefreshReport>, StorageError> {
        if reuse.root_identity != self.root_identity {
            return Ok(None);
        }
        let head = reuse.head_commit.to_owned();
        let parameters = reuse.parameters;
        let shallow = reuse.shallow_history;
        self.state(None, 0, 0, move |transaction, budget| {
            let Some(manifest) = dataset::manifest(transaction, NAMESPACE, NAME, budget)? else {
                return Ok(None);
            };
            let metadata: Metadata =
                serde_json::from_value(manifest.metadata).map_err(|_| corrupt())?;
            if metadata.report.head_commit != head
                || metadata.report.shallow_history != shallow
                || metadata.parameters != Some(parameters)
            {
                return Ok(None);
            }
            let mut report = metadata.report;
            report.reused = true;
            Ok(Some(report))
        })
        .await
    }

    /// Atomically clear the history pointer without exposing partial replacements.
    /// # Errors
    /// Rejects project mismatch or failed native publication.
    pub async fn clear_file_history(&self, project: &ProjectId) -> Result<(), StorageError> {
        self.require_project(project)?;
        self.clear_dataset(NAMESPACE, NAME).await.map(|_| ())
    }

    /// Read current-file churn with complete filtering before the response limit.
    /// # Errors
    /// Rejects project/limit mismatch, corrupt facts, or exhausted native reads.
    pub async fn current_file_history(
        &self,
        request: FileHistoryQuery<'_>,
    ) -> Result<Vec<FileHistoryRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_query_limit(request.limit)?;
        let path = request.path.cloned();
        let minimum = request.minimum_commits;
        let limit = request.limit;
        self.state(None, 0, 0, move |transaction, budget| {
            let Some(manifest) = dataset::manifest(transaction, NAMESPACE, NAME, budget)? else {
                return Ok(Vec::new());
            };
            let metadata: Metadata =
                serde_json::from_value(manifest.metadata.clone()).map_err(|_| corrupt())?;
            let live = live_paths(transaction, budget)?;
            let mut output = Vec::new();
            dataset::visit::<HistoryRow>(
                transaction,
                NAMESPACE,
                NAME,
                &manifest,
                budget,
                |row, _| {
                    let HistoryRow::File(fact) = row else {
                        return Ok(());
                    };
                    validate_fact(&fact)?;
                    if !live.contains(fact.path.as_str())
                        || path.as_ref().is_some_and(|path| path != &fact.path)
                        || fact.commit_count < u64::from(minimum)
                    {
                        return Ok(());
                    }
                    let timestamp = fact
                        .last_touched_at
                        .map(|seconds| {
                            i64::try_from(seconds)
                                .map_err(|_| corrupt())
                                .and_then(|seconds| {
                                    jiff::Timestamp::from_second(seconds)
                                        .map(|timestamp| timestamp.to_string())
                                        .map_err(|_| corrupt())
                                })
                        })
                        .transpose()?;
                    retain(
                        &mut output,
                        FileHistoryRecord {
                            path: fact.path.as_str().to_owned(),
                            head_commit: metadata.report.head_commit.clone(),
                            commit_count: fact.commit_count,
                            author_count: fact.author_count,
                            insertions: fact.insertions,
                            deletions: fact.deletions,
                            last_touched_at: timestamp,
                            shallow_history: metadata.report.shallow_history,
                        },
                        usize::from(limit),
                        |a, b| {
                            b.commit_count
                                .cmp(&a.commit_count)
                                .then_with(|| b.last_touched_at.cmp(&a.last_touched_at))
                                .then(a.path.cmp(&b.path))
                        },
                    );
                    Ok(())
                },
            )?;
            Ok(output)
        })
        .await
    }

    /// Read co-change partners that still exist in the complete current graph.
    /// # Errors
    /// Rejects project/limit mismatch, corrupt ratios, or exhausted native reads.
    pub async fn current_file_cochanges(
        &self,
        request: FileCochangeQuery<'_>,
    ) -> Result<Vec<FileCochangeRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_query_limit(request.limit)?;
        let (path, minimum, limit) = (request.path.clone(), request.minimum_commits, request.limit);
        self.state(None, 0, 0, move |transaction, budget| {
            let Some(manifest) = dataset::manifest(transaction, NAMESPACE, NAME, budget)? else {
                return Ok(Vec::new());
            };
            let live = live_paths(transaction, budget)?;
            let counts = file_counts(transaction, budget, &manifest)?;
            let Some(anchor) = counts.get(path.as_str()) else {
                return Ok(Vec::new());
            };
            let mut output = Vec::new();
            dataset::visit::<HistoryRow>(
                transaction,
                NAMESPACE,
                NAME,
                &manifest,
                budget,
                |row, _| {
                    let HistoryRow::Cochange(fact) = row else {
                        return Ok(());
                    };
                    let partner = if fact.path_a == path {
                        &fact.path_b
                    } else if fact.path_b == path {
                        &fact.path_a
                    } else {
                        return Ok(());
                    };
                    if !live.contains(partner.as_str()) || fact.commit_count < u64::from(minimum) {
                        return Ok(());
                    }
                    let Some(partner_count) = counts.get(partner.as_str()) else {
                        return Ok(());
                    };
                    let shared = fact.commit_count.to_f32().ok_or(corrupt())?;
                    if !fact.confidence.is_finite()
                        || !(0.0..=1.0).contains(&fact.confidence)
                        || fact.commit_count > *anchor
                        || fact.commit_count > *partner_count
                    {
                        return Err(corrupt());
                    }
                    let record = FileCochangeRecord {
                        path: partner.as_str().to_owned(),
                        shared_commits: fact.commit_count,
                        jaccard: fact.confidence,
                        anchor_ratio: shared / (*anchor).max(1).to_f32().ok_or(corrupt())?,
                        partner_ratio: shared / (*partner_count).max(1).to_f32().ok_or(corrupt())?,
                        anchor_commits: *anchor,
                        partner_commits: *partner_count,
                    };
                    retain(&mut output, record, usize::from(limit), |a, b| {
                        b.shared_commits
                            .cmp(&a.shared_commits)
                            .then(b.jaccard.total_cmp(&a.jaccard))
                            .then(a.path.cmp(&b.path))
                    });
                    Ok(())
                },
            )?;
            Ok(output)
        })
        .await
    }
}

fn live_paths(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<BTreeSet<String>, RocksError> {
    let mut paths = BTreeSet::new();
    if transaction.current()?.is_none() {
        return Ok(paths);
    }
    let snapshot = transaction.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let mut bytes = 0_usize;
    visit_files(&graph, budget, |file, _| {
        bytes = bytes.saturating_add(file.normalized_path.len() + 128);
        if bytes > 16 * 1_048_576 {
            return Err(RocksError::Limit {
                resource: "history_file_workspace",
            });
        }
        paths.insert(file.normalized_path);
        Ok(())
    })?;
    Ok(paths)
}

fn file_counts(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    manifest: &dataset::Manifest,
) -> Result<BTreeMap<String, u64>, RocksError> {
    let mut counts = BTreeMap::new();
    let mut bytes = 0_usize;
    dataset::visit::<HistoryRow>(transaction, NAMESPACE, NAME, manifest, budget, |row, _| {
        if let HistoryRow::File(fact) = row {
            validate_fact(&fact)?;
            bytes = bytes.saturating_add(fact.path.as_str().len() + 128);
            if bytes > 24 * 1_048_576 {
                return Err(RocksError::Limit {
                    resource: "history_file_workspace",
                });
            }
            counts.insert(fact.path.as_str().to_owned(), fact.commit_count);
        }
        Ok(())
    })?;
    Ok(counts)
}

fn validate_fact(fact: &FileHistoryFact) -> Result<(), RocksError> {
    if fact.author_count > fact.commit_count
        || (fact.commit_count == 0) != fact.last_touched_at.is_none()
    {
        return Err(corrupt());
    }
    Ok(())
}

pub(crate) fn visit_metrics(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    mut visitor: impl FnMut(&str, super::FileHistoryMetrics) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let Some(manifest) = dataset::manifest(transaction, NAMESPACE, NAME, budget)? else {
        return Ok(());
    };
    dataset::visit::<HistoryRow>(transaction, NAMESPACE, NAME, &manifest, budget, |row, _| {
        if let HistoryRow::File(fact) = row {
            validate_fact(&fact)?;
            visitor(
                fact.path.as_str(),
                super::FileHistoryMetrics {
                    commit_count: fact.commit_count,
                    author_count: fact.author_count,
                    insertions: fact.insertions,
                    deletions: fact.deletions,
                    last_touched_at: fact.last_touched_at,
                },
            )?;
        }
        Ok(())
    })
}

fn corrupt() -> RocksError {
    RocksError::Corrupt { field: "history" }
}
