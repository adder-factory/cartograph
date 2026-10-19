//! Native project status uses exact manifest counts and the same retained snapshot set.

use super::{
    GenerationCounts, GenerationSourceAdmission, GenerationStorageSummary,
    ProjectCurrentGeneration, ProjectSnapshot, validate_root_identity,
};
use crate::{
    NativeDatabase, StorageError,
    retrieval::native::visit_files,
    rocks::{GraphInfo, ReadBudget, RevisionState, RocksError, StateTransaction, StoredRevision},
};
use cartograph_domain::ProjectId;
use std::time::Duration;

impl NativeDatabase {
    /// Read an owned project identity and its complete current publication.
    /// # Errors
    /// Rejects malformed identity, corrupt manifests, or exhausted native reads.
    pub async fn project_snapshot_by_root(
        &self,
        root: &str,
    ) -> Result<Option<ProjectSnapshot>, StorageError> {
        validate_root_identity(root)?;
        if root != self.root_identity {
            return Ok(None);
        }
        let project = self.project_id.clone();
        self.state(None, 0, 0, move |transaction, budget| {
            let current = transaction
                .current()?
                .map(|revision| {
                    let snapshot = transaction.graph_snapshot()?;
                    let graph = snapshot.graph(budget)?;
                    Ok(ProjectCurrentGeneration {
                        generation_id: revision.generation_id,
                        sequence: i64::try_from(revision.sequence).map_err(|_| corrupt())?,
                        source_revision: revision.source_revision.to_string(),
                        content_digest: revision.content_digest.ok_or(corrupt())?,
                        digest_version: revision.digest_version.ok_or(corrupt())?,
                        counts: counts(graph.info())?,
                        source_admission: GenerationSourceAdmission {
                            run_exclude_patterns: u16::try_from(revision.run_excludes.len())
                                .map_err(|_| corrupt())?,
                            run_excludes: revision.run_excludes,
                            maximum_file_bytes: revision.maximum_file_bytes,
                        },
                    })
                })
                .transpose()?;
            Ok(Some(ProjectSnapshot {
                project_id: project,
                current,
                generation_storage: summary(transaction, budget)?,
            }))
        })
        .await
    }

    /// Check this owned project's current pointer without a source scan or mutation.
    /// # Errors
    /// Rejects malformed identity/deadlines and bounded native read failures.
    pub async fn root_has_current_generation(
        &self,
        root: &str,
        timeout: Duration,
    ) -> Result<bool, StorageError> {
        validate_root_identity(root)?;
        if root != self.root_identity {
            return Ok(false);
        }
        self.state_with_timeout(None, 0, 0, timeout, |transaction, _| {
            Ok(transaction.current()?.is_some())
        })
        .await
    }

    /// Count every durable revision and retained published source byte lower bound.
    /// # Errors
    /// Rejects mismatched project, corrupt metadata, arithmetic overflow, or read bounds.
    pub async fn generation_storage_summary(
        &self,
        project: &ProjectId,
    ) -> Result<GenerationStorageSummary, StorageError> {
        self.require_project(project)?;
        self.state(None, 0, 0, summary).await
    }
}

pub(crate) fn revisions(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<Vec<StoredRevision>, RocksError> {
    let mut cursor = None;
    let mut output = Vec::new();
    loop {
        let (revisions, next) = transaction.revisions(cursor, 256, budget)?;
        if output.len().saturating_add(revisions.len()) > 8192 {
            return Err(RocksError::Limit {
                resource: "revision_inventory",
            });
        }
        output.extend(revisions);
        cursor = next;
        if cursor.is_none() {
            return Ok(output);
        }
    }
}
fn summary(
    transaction: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<GenerationStorageSummary, RocksError> {
    let mut output = GenerationStorageSummary::default();
    let now = jiff::Timestamp::now().as_second();
    for revision in revisions(transaction, budget)? {
        match revision.state {
            RevisionState::Staging => output.staging += 1,
            RevisionState::Ready => output.ready += 1,
            RevisionState::Current => output.current += 1,
            RevisionState::Superseded => output.superseded += 1,
            RevisionState::Failed => output.failed += 1,
        }
        if matches!(
            revision.state,
            RevisionState::Superseded | RevisionState::Failed
        ) && let Some(reserved) = &revision.reserved_at
        {
            let timestamp = reserved.parse::<jiff::Timestamp>().map_err(|_| corrupt())?;
            let age = u64::try_from(now.saturating_sub(timestamp.as_second()).max(0))
                .map_err(|_| corrupt())?;
            output.oldest_terminal_age_seconds = output.oldest_terminal_age_seconds.max(age);
        }
        if matches!(
            revision.state,
            RevisionState::Current | RevisionState::Superseded
        ) {
            match transaction.retained_graph_snapshot(&revision.generation_id) {
                Ok(snapshot) => visit_files(&snapshot.graph(budget)?, budget, |file, _| {
                    output.source_bytes = output
                        .source_bytes
                        .checked_add(file.byte_size)
                        .ok_or(corrupt())?;
                    Ok(())
                })?,
                Err(RocksError::GenerationUnavailable) => {}
                Err(error) => return Err(error),
            }
        }
    }
    output.estimated_retained_bytes = output.source_bytes;
    Ok(output)
}
fn counts(info: &GraphInfo) -> Result<GenerationCounts, RocksError> {
    let count = |n| i64::try_from(n).map_err(|_| corrupt());
    Ok(GenerationCounts {
        files: count(info.files)?,
        symbols: count(info.symbols)?,
        edges: count(info.edges)?,
        references: count(info.references)?,
        numerical_sites: count(info.numerical_sites)?,
        documents: count(info.documents)?,
    })
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "native_project_metadata",
    }
}
