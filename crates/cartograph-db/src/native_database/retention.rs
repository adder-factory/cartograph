//! Bounded cold-orphan collection in the same snapshot as the current graph.

use super::{NativeDatabase, decode, storage_error};
use crate::{
    AgentArtifactKind, AgentArtifactScope, RegisteredEmbeddingModel, StorageError,
    artifacts::native::{bump_epoch, decode_primary, is_current, remove},
    retrieval::native::visit_files,
    rocks::{
        GraphSnapshot, ReadBudget, RocksError, StateNamespace as Ns, StateTransaction,
        StorageRequest,
    },
    semantic::native::{feature_error, vectors},
};
use cartograph_domain::{DocumentId, GenerationId, ModelId};
use serde::Serialize;
use std::collections::BTreeSet;

const PAGE: u16 = 64;
const GRACE_SECONDS: i64 = 600;

/// Committed, bounded progress; continuation requires another maintenance call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeDerivedPruneReport {
    /// Physical queue/vector/artifact records inspected in this batch.
    pub scanned: u32,
    /// Obsolete generation demand removed immediately.
    pub queue_rows_removed: u32,
    /// Cold source-incompatible vectors removed after their grace period.
    pub vectors_removed: u32,
    /// Cold role/summary records removed with their UUID and scope indexes.
    pub artifacts_removed: u32,
    /// Newly observed orphans retained for their ten minute grace period.
    pub grace_started: u32,
    /// At least one namespace has an unvisited continuation page.
    pub incomplete: bool,
}

impl NativeDatabase {
    /// Collect at most 64 records per derived family under the caller's control.
    /// Notes and session records are never automatic deletion candidates.
    /// # Errors
    /// Fails closed on stopped requests, malformed state, or atomic write failure.
    pub async fn prune_derived_state_with_request(
        &self,
        request: &StorageRequest,
    ) -> Result<NativeDerivedPruneReport, StorageError> {
        request.check().map_err(storage_error)?;
        self.state_with_request(None, 4 * 1_048_576, 1024, request, |tx, budget| {
            let timestamp = jiff::Timestamp::now().as_second();
            collect(tx, budget, timestamp)
        })
        .await
    }
}

fn collect(
    tx: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    timestamp: i64,
) -> Result<NativeDerivedPruneReport, RocksError> {
    let mut report = NativeDerivedPruneReport::default();
    let Some(current) = tx.current()? else {
        return Ok(report);
    };
    let snapshot = tx.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let mut cursors = Vec::new();
    let queue = page(
        tx,
        budget,
        Ns::Derived,
        b"q/",
        b"gc/queue_cursor",
        &mut report,
        &mut cursors,
    )?;
    let vectors = page(
        tx,
        budget,
        Ns::Embeddings,
        b"v/",
        b"gc/vector_cursor",
        &mut report,
        &mut cursors,
    )?;
    let artifacts = page(
        tx,
        budget,
        Ns::Artifacts,
        b"a/",
        b"gc/artifact_cursor",
        &mut report,
        &mut cursors,
    )?;
    collect_queue(tx, &current.generation_id, &mut report, queue)?;
    collect_vectors(
        tx,
        budget,
        &graph,
        &current.generation_id,
        timestamp,
        &mut report,
        vectors,
    )?;
    collect_artifacts(
        tx,
        budget,
        &graph,
        &current.generation_id,
        timestamp,
        &mut report,
        artifacts,
    )?;
    for (key, next) in cursors {
        if let Some(next) = next {
            tx.put(Ns::Metadata, &key, next)?;
        } else {
            tx.delete(Ns::Metadata, &key)?;
        }
    }
    Ok(report)
}

type Rows = Vec<(crate::rocks::RecordKey, Vec<u8>)>;
type CursorUpdates = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn page(
    tx: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    namespace: Ns,
    prefix: &[u8],
    cursor: &[u8],
    report: &mut NativeDerivedPruneReport,
    cursors: &mut CursorUpdates,
) -> Result<Rows, RocksError> {
    let after = tx.get(Ns::Metadata, cursor, budget)?;
    let page = tx.scan(namespace, prefix, after.as_deref(), PAGE, budget)?;
    report.scanned += u32::try_from(page.rows.len()).map_err(|_| corrupt())?;
    report.incomplete |= page.next_after.is_some();
    cursors.push((
        cursor.to_vec(),
        page.next_after.map(|key| key.as_bytes().to_vec()),
    ));
    Ok(page.rows)
}

fn collect_queue(
    tx: &mut StateTransaction<'_>,
    generation: &GenerationId,
    report: &mut NativeDerivedPruneReport,
    rows: Rows,
) -> Result<(), RocksError> {
    let current = format!("q/{generation}/");
    for (key, _) in rows {
        if !key.as_bytes().starts_with(current.as_bytes()) {
            tx.delete(Ns::Derived, key.as_bytes())?;
            report.queue_rows_removed += 1;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct OrphanCandidate<'a> {
    family: &'a str,
    key: &'a [u8],
    bytes: &'a [u8],
    incompatible: bool,
}

fn cold(
    tx: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    candidate: OrphanCandidate<'_>,
    timestamp: i64,
    report: &mut NativeDerivedPruneReport,
) -> Result<bool, RocksError> {
    let OrphanCandidate {
        family,
        key,
        bytes,
        incompatible,
    } = candidate;
    let text = std::str::from_utf8(key).map_err(|_| corrupt())?;
    let marker = format!("gc/orphan/{family}/{text}");
    let first = tx.get(Ns::Metadata, marker.as_bytes(), budget)?;
    if !incompatible {
        if first.is_some() {
            tx.delete(Ns::Metadata, marker.as_bytes())?;
        }
        return Ok(false);
    }
    let fingerprint = blake3::hash(bytes);
    let unchanged = first
        .as_deref()
        .filter(|stored| stored.len() == 40 && stored[8..] == fingerprint.as_bytes()[..]);
    let Some(first) = unchanged else {
        let mut marker_value = timestamp.to_be_bytes().to_vec();
        marker_value.extend_from_slice(fingerprint.as_bytes());
        tx.put(Ns::Metadata, marker.as_bytes(), marker_value)?;
        report.grace_started += 1;
        return Ok(false);
    };
    let first = i64::from_be_bytes(first[..8].try_into().map_err(|_| corrupt())?);
    if timestamp.saturating_sub(first) < GRACE_SECONDS {
        return Ok(false);
    }
    tx.delete(Ns::Metadata, marker.as_bytes())?;
    Ok(true)
}

pub(crate) fn forget_orphan(
    tx: &mut StateTransaction<'_>,
    family: &str,
    key: &[u8],
) -> Result<(), RocksError> {
    let text = std::str::from_utf8(key).map_err(|_| corrupt())?;
    tx.delete(
        Ns::Metadata,
        format!("gc/orphan/{family}/{text}").as_bytes(),
    )
}

fn collect_vectors(
    tx: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    graph: &GraphSnapshot<'_, '_>,
    generation: &GenerationId,
    timestamp: i64,
    report: &mut NativeDerivedPruneReport,
    rows: Rows,
) -> Result<(), RocksError> {
    for (key, bytes) in rows {
        budget.checkpoint()?;
        let key_text = std::str::from_utf8(key.as_bytes()).map_err(|_| corrupt())?;
        let mut parts = key_text.split('/');
        if parts.next() != Some("v") {
            return Err(corrupt());
        }
        let model_id = ModelId::parse(parts.next().ok_or(corrupt())?).map_err(|_| corrupt())?;
        let document_id =
            DocumentId::parse(parts.next().ok_or(corrupt())?).map_err(|_| corrupt())?;
        if parts.next().is_some() {
            return Err(corrupt());
        }
        let model: RegisteredEmbeddingModel = tx
            .get(Ns::Embeddings, format!("m/{model_id}").as_bytes(), budget)?
            .map(|bytes| decode(&bytes))
            .transpose()?
            .ok_or(corrupt())?;
        if model.selector.model_id != model_id {
            return Err(corrupt());
        }
        let (digest, _) =
            vectors::decode_vector(&bytes, model.selector.dimension).map_err(semantic_error)?;
        let incompatible = if let Some(document) = graph.document(&document_id, budget)? {
            let (_, expected) =
                vectors::source(tx, graph, generation, document, budget).map_err(semantic_error)?;
            digest != expected
        } else {
            true
        };
        if cold(
            tx,
            budget,
            OrphanCandidate {
                family: "vector",
                key: key.as_bytes(),
                bytes: &bytes,
                incompatible,
            },
            timestamp,
            report,
        )? {
            tx.delete(Ns::Embeddings, key.as_bytes())?;
            report.vectors_removed += 1;
        }
    }
    if report.vectors_removed > 0 {
        bump_epoch(tx, budget, b"vector_epoch")?;
    }
    Ok(())
}

fn collect_artifacts(
    tx: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    graph: &GraphSnapshot<'_, '_>,
    generation: &GenerationId,
    timestamp: i64,
    report: &mut NativeDerivedPruneReport,
    rows: Rows,
) -> Result<(), RocksError> {
    // Retain only the scopes in this bounded page, never the project's full
    // path inventory. Notes and sessions cannot trigger a source scan.
    let mut missing_files = BTreeSet::new();
    let mut missing_modules = BTreeSet::new();
    for (key, bytes) in &rows {
        let record = decode_primary(key.as_bytes(), bytes)?;
        if matches!(
            record.kind(),
            AgentArtifactKind::Role | AgentArtifactKind::Summary
        ) {
            match record.scope() {
                AgentArtifactScope::File => {
                    missing_files.insert(record.scope_key().to_owned());
                }
                AgentArtifactScope::Module => {
                    missing_modules.insert(record.scope_key().to_owned());
                }
                _ => {}
            }
        }
    }
    if !missing_files.is_empty() || !missing_modules.is_empty() {
        visit_files(graph, budget, |file, _| {
            missing_files.remove(&file.normalized_path);
            let module = file
                .normalized_path
                .rsplit_once('/')
                .map_or(".", |(parent, _)| parent);
            missing_modules.remove(module);
            Ok(())
        })?;
    }
    for (key, bytes) in rows {
        let record = decode_primary(key.as_bytes(), &bytes)?;
        if !matches!(
            record.kind(),
            AgentArtifactKind::Role | AgentArtifactKind::Summary
        ) {
            continue;
        }
        // A live file/module scope has only one indexed role/summary, which can
        // be refreshed in place. Keep it until the scope itself disappears.
        let compatible = match record.scope() {
            AgentArtifactScope::Symbol => {
                is_current(&record, Some(generation), Some(graph), budget)?
            }
            AgentArtifactScope::File => !missing_files.contains(record.scope_key()),
            AgentArtifactScope::Module => !missing_modules.contains(record.scope_key()),
            AgentArtifactScope::Project | AgentArtifactScope::Session => true,
        };
        if cold(
            tx,
            budget,
            OrphanCandidate {
                family: "artifact",
                key: key.as_bytes(),
                bytes: &bytes,
                incompatible: !compatible,
            },
            timestamp,
            report,
        )? {
            remove(tx, budget, record.id())?;
            report.artifacts_removed += 1;
        }
    }
    Ok(())
}

fn semantic_error(error: crate::SemanticStorageError) -> RocksError {
    match feature_error(error) {
        StorageError::CorruptStoredValue { field } => RocksError::Corrupt { field },
        _ => RocksError::Operation {
            operation: "orphan-vector-source",
        },
    }
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "derived-retention",
    }
}

#[cfg(test)]
mod tests;
