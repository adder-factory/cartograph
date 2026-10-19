//! Artifact identities, scope indexes, and source fences share one durable batch.

mod backlog;
mod neighbor;
mod readiness;
mod rollup;
mod structural_roles;
pub(crate) mod summaries;

use super::{
    AgentArtifactKind as Kind, AgentArtifactQuery, AgentArtifactRecord as Record,
    AgentArtifactScope as Scope, AgentArtifactState as State, AgentRoleCount, CurrentModuleSummary,
    CurrentModuleSummaryPage, NewAgentArtifact, canonical_uuid, validate_artifact, validate_body,
    validate_metadata, validate_scope_key,
};
use crate::{
    NativeDatabase, StorageError,
    native_database::{decode, encode, now, timestamp_order, visit_state},
    retrieval::native::retain,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use cartograph_domain::{ArtifactId, ContentDigest, GenerationId, ProjectId, SymbolId};
use num_traits::ToPrimitive as _;
use std::collections::BTreeMap;

const NAMESPACE: StateNamespace = StateNamespace::Artifacts;
const WRITE_BYTES: u32 = 1_048_576;
const WRITE_ROWS: u32 = 8;

impl NativeDatabase {
    /// Create an artifact and all public identity indexes atomically.
    /// # Errors
    /// Rejects invalid input, a live scoped collision, or native write failure.
    pub async fn create_agent_artifact(
        &self,
        project: &ProjectId,
        artifact: &NewAgentArtifact,
    ) -> Result<Record, StorageError> {
        self.write_artifact(project, artifact, false).await
    }

    /// Replace one non-archived role or summary, retaining its public identity.
    /// # Errors
    /// Rejects invalid project, input, unsupported kind, or atomic write failure.
    pub async fn replace_scoped_agent_artifact(
        &self,
        project: &ProjectId,
        artifact: &NewAgentArtifact,
    ) -> Result<Record, StorageError> {
        if !matches!(artifact.kind, Kind::Role | Kind::Summary) {
            return Err(StorageError::InvalidInput {
                field: "artifact_kind",
            });
        }
        self.write_artifact(project, artifact, true).await
    }

    async fn write_artifact(
        &self,
        project: &ProjectId,
        artifact: &NewAgentArtifact,
        replace: bool,
    ) -> Result<Record, StorageError> {
        self.require_project(project)?;
        validate_artifact(artifact)?;
        let artifact = artifact.clone();
        self.state(None, WRITE_BYTES, WRITE_ROWS, move |transaction, budget| {
            write(transaction, budget, artifact, replace)
        })
        .await
    }

    /// List newest matching artifacts; source-dependent filters use the same snapshot.
    /// # Errors
    /// Rejects invalid project, malformed state, or bounded read exhaustion.
    pub async fn list_agent_artifacts(
        &self,
        project: &ProjectId,
        request: AgentArtifactQuery<'_>,
    ) -> Result<Vec<Record>, StorageError> {
        self.require_project(project)?;
        let (scope_key, body, note_kind) = (
            request.scope_key.map(str::to_owned),
            request.body_equals.map(str::to_owned),
            request.note_kind.map(str::to_owned),
        );
        self.state(None, 0, 0, move |transaction, budget| {
            let current = transaction
                .current()?
                .map(|revision| revision.generation_id);
            let snapshot = if request.current_generation_only && current.is_some() {
                Some(transaction.graph_snapshot()?)
            } else {
                None
            };
            let graph = snapshot
                .as_ref()
                .map(|snapshot| snapshot.graph(budget))
                .transpose()?;
            let mut output = Vec::new();
            visit_state(
                transaction,
                NAMESPACE,
                b"a/",
                budget,
                |_, record: Record, budget| {
                    validate_record(&record)?;
                    if request.kind.is_some_and(|value| record.kind != value)
                        || request.scope.is_some_and(|value| record.scope != value)
                        || request.state.is_some_and(|value| record.state != value)
                        || scope_key
                            .as_ref()
                            .is_some_and(|value| &record.scope_key != value)
                        || body.as_ref().is_some_and(|value| &record.body != value)
                        || note_kind.as_deref().is_some_and(|value| {
                            record
                                .metadata
                                .get("noteKind")
                                .and_then(serde_json::Value::as_str)
                                != Some(value)
                        })
                        || request.since_unix_ms.is_some_and(|value| {
                            timestamp_ms(&record.updated_at)
                                .is_none_or(|timestamp| timestamp < value)
                        })
                        || (request.current_generation_only
                            && !is_current(&record, current.as_ref(), graph.as_ref(), budget)?)
                    {
                        return Ok(());
                    }
                    retain(&mut output, record, usize::from(request.limit), newest);
                    Ok(())
                },
            )?;
            Ok(output)
        })
        .await
    }

    /// Delete a stable artifact identity and its indexes in one durable batch.
    /// # Errors
    /// Rejects malformed identity, project mismatch, or inconsistent state.
    pub async fn delete_agent_artifact(
        &self,
        project: &ProjectId,
        id: &str,
    ) -> Result<bool, StorageError> {
        self.require_project(project)?;
        if !canonical_uuid(id) {
            return Err(StorageError::InvalidInput {
                field: "artifact_id",
            });
        }
        let key = format!("u/{}", id.to_ascii_lowercase());
        self.state(None, WRITE_BYTES, WRITE_ROWS, move |transaction, budget| {
            let Some(bytes) = transaction.get(NAMESPACE, key.as_bytes(), budget)? else {
                return Ok(false);
            };
            remove(transaction, budget, counter(&bytes)?)
        })
        .await
    }

    /// Delete a legacy numeric artifact identity with the same atomic semantics.
    /// # Errors
    /// Rejects invalid identity, project mismatch, or inconsistent state.
    pub async fn delete_agent_artifact_by_id(
        &self,
        project: &ProjectId,
        id: u64,
    ) -> Result<bool, StorageError> {
        self.require_project(project)?;
        if id == 0 || id > i64::MAX as u64 {
            return Err(StorageError::InvalidInput {
                field: "artifact_id",
            });
        }
        self.state(None, WRITE_BYTES, WRITE_ROWS, move |transaction, budget| {
            remove(transaction, budget, id)
        })
        .await
    }

    /// Count roles whose exact source digest still matches the current graph.
    /// # Errors
    /// Rejects project mismatch, corrupt artifacts, or bounded read exhaustion.
    pub async fn agent_role_distribution(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<AgentRoleCount>, StorageError> {
        self.require_project(project)?;
        self.state(None, 0, 0, |transaction, budget| {
            let Some(current) = transaction.current()? else {
                return Ok(Vec::new());
            };
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let mut counts = BTreeMap::<String, u64>::new();
            visit_state(
                transaction,
                NAMESPACE,
                b"a/",
                budget,
                |_, record: Record, budget| {
                    validate_record(&record)?;
                    if record.kind == Kind::Role
                        && record.scope == Scope::Symbol
                        && matches!(record.state, State::Active | State::Complete)
                        && is_current(&record, Some(&current.generation_id), Some(&graph), budget)?
                    {
                        *counts.entry(record.body).or_default() += 1;
                        if counts.len() > 8192 {
                            return Err(workspace());
                        }
                    }
                    Ok(())
                },
            )?;
            let mut output = counts
                .into_iter()
                .map(|(role, symbols)| AgentRoleCount { role, symbols })
                .collect::<Vec<_>>();
            output.sort_by(|a, b| b.symbols.cmp(&a.symbols).then(a.role.cmp(&b.role)));
            Ok(output)
        })
        .await
    }

    /// Read bounded current-generation file paragraphs by exact normalized path.
    /// # Errors
    /// Rejects invalid path sets, project mismatch, or corrupt native state.
    pub async fn current_file_summary_texts(
        &self,
        project: &ProjectId,
        paths: &[String],
    ) -> Result<BTreeMap<String, String>, StorageError> {
        self.require_project(project)?;
        if paths.is_empty()
            || paths.len() > super::MAX_INLINE_FILE_SUMMARIES
            || paths
                .iter()
                .any(|path| cartograph_domain::NormalizedPath::parse(path).is_err())
        {
            return Err(StorageError::InvalidInput {
                field: "summary_paths",
            });
        }
        let paths = paths.to_vec();
        self.state(None, 0, 0, move |transaction, budget| {
            let Some(current) = transaction.current()? else {
                return Ok(BTreeMap::new());
            };
            let mut output = BTreeMap::new();
            for path in paths {
                if let Some(record) =
                    read_scope(transaction, Kind::Summary, Scope::File, &path, budget)?
                    && record.state == State::Complete
                    && record.generation_id.as_deref() == Some(current.generation_id.as_str())
                {
                    output.insert(path, record.body);
                }
            }
            Ok(output)
        })
        .await
    }

    /// List current module summaries in normalized directory order.
    /// # Errors
    /// Rejects invalid bounds, project mismatch, or malformed native artifacts.
    pub async fn current_module_summaries(
        &self,
        project: &ProjectId,
        directory: Option<&cartograph_domain::NormalizedPath>,
        limit: u16,
    ) -> Result<CurrentModuleSummaryPage, StorageError> {
        self.require_project(project)?;
        super::validate_limit(limit, super::MAX_ARTIFACT_LIMIT)?;
        let directory = directory.map(|path| path.as_str().to_owned());
        self.state(None, 0, 0, move |transaction, budget| {
            let current = transaction
                .current()?
                .map(|revision| revision.generation_id);
            let mut summaries = Vec::new();
            let mut total = 0_u64;
            visit_state(
                transaction,
                NAMESPACE,
                b"a/",
                budget,
                |_, record: Record, _| {
                    validate_record(&record)?;
                    if record.kind == Kind::Summary
                        && record.scope == Scope::Module
                        && record.state == State::Complete
                        && record.source_digest.is_some()
                        && current
                            .as_ref()
                            .is_some_and(|id| record.generation_id.as_deref() == Some(id.as_str()))
                        && directory.as_ref().is_none_or(|path| {
                            record.scope_key == *path
                                || record.scope_key.starts_with(&format!("{path}/"))
                        })
                    {
                        total += 1;
                        retain(
                            &mut summaries,
                            CurrentModuleSummary {
                                directory: record.scope_key,
                                summary: record.body,
                                metadata: record.metadata,
                            },
                            usize::from(limit),
                            |a, b| a.directory.cmp(&b.directory),
                        );
                    }
                    Ok(())
                },
            )?;
            Ok(CurrentModuleSummaryPage {
                truncated: total > summaries.len() as u64,
                total,
                summaries,
            })
        })
        .await
    }
}

fn primary(id: u64) -> String {
    format!("a/{id:016x}")
}
fn scope_key(kind: Kind, scope: Scope, key: &str) -> String {
    format!(
        "s/{}/{}/{}",
        kind.as_str(),
        scope.as_str(),
        blake3::hash(key.as_bytes()).to_hex()
    )
}

fn indexed(kind: Kind, state: State) -> bool {
    matches!(kind, Kind::Role | Kind::Summary) && state != State::Archived
}

pub(crate) fn read_scope(
    transaction: &StateTransaction<'_>,
    kind: Kind,
    scope: Scope,
    key: &str,
    budget: &mut ReadBudget,
) -> Result<Option<Record>, RocksError> {
    let Some(bytes) = transaction.get(NAMESPACE, scope_key(kind, scope, key).as_bytes(), budget)?
    else {
        return Ok(None);
    };
    let id = counter(&bytes)?;
    let record: Record = transaction
        .get(NAMESPACE, primary(id).as_bytes(), budget)?
        .map(|bytes| decode(&bytes))
        .transpose()?
        .ok_or(corrupt())?;
    validate_record(&record)?;
    if record.id != id
        || record.kind != kind
        || record.scope != scope
        || record.scope_key != key
        || !indexed(record.kind, record.state)
    {
        return Err(corrupt());
    }
    Ok(Some(record))
}

fn write(
    transaction: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    input: NewAgentArtifact,
    replace: bool,
) -> Result<Record, RocksError> {
    let existing = if indexed(input.kind, input.state) {
        read_scope(
            transaction,
            input.kind,
            input.scope,
            &input.scope_key,
            budget,
        )?
    } else {
        None
    };
    if existing.is_some() && !replace {
        return Err(RocksError::ReplayConflict);
    }
    let timestamp = now();
    let (id, artifact_id, created_at) = if let Some(record) = existing {
        (record.id, record.artifact_id, record.created_at)
    } else {
        let sequence = transaction
            .get(StateNamespace::Metadata, b"artifact_sequence", budget)?
            .map(|bytes| counter(&bytes))
            .transpose()?
            .unwrap_or(0);
        let next = sequence
            .checked_add(1)
            .filter(|value| i64::try_from(*value).is_ok())
            .ok_or(corrupt())?;
        let mut entropy = [0_u8; 16];
        getrandom::fill(&mut entropy).map_err(|_| RocksError::Operation {
            operation: "artifact-identity",
        })?;
        let identity = ArtifactId::from_uuid_v8(entropy).to_string();
        if transaction
            .get(NAMESPACE, format!("u/{identity}").as_bytes(), budget)?
            .is_some()
        {
            return Err(RocksError::ReplayConflict);
        }
        transaction.put(
            StateNamespace::Metadata,
            b"artifact_sequence",
            next.to_be_bytes().to_vec(),
        )?;
        (next, identity, timestamp.clone())
    };
    let record = Record {
        id,
        artifact_id,
        kind: input.kind,
        scope: input.scope,
        scope_key: input.scope_key,
        body: input.body,
        metadata: input.metadata,
        generation_id: input.generation_id.map(|value| value.to_string()),
        source_digest: input.source_digest.map(|value| value.to_string()),
        state: input.state,
        created_at,
        updated_at: timestamp,
    };
    validate_record(&record)?;
    transaction.put(NAMESPACE, primary(id).as_bytes(), encode(&record)?)?;
    transaction.put(
        NAMESPACE,
        format!("u/{}", record.artifact_id).as_bytes(),
        id.to_be_bytes().to_vec(),
    )?;
    if indexed(record.kind, record.state) {
        transaction.put(
            NAMESPACE,
            scope_key(record.kind, record.scope, &record.scope_key).as_bytes(),
            id.to_be_bytes().to_vec(),
        )?;
    }
    if record.kind == Kind::Summary {
        bump_epoch(transaction, budget, b"summary_epoch")?;
    }
    Ok(record)
}

pub(crate) fn decode_primary(key: &[u8], bytes: &[u8]) -> Result<Record, RocksError> {
    let record: Record = decode(bytes)?;
    validate_record(&record)?;
    if primary(record.id).as_bytes() != key {
        return Err(corrupt());
    }
    Ok(record)
}

pub(crate) fn remove(
    transaction: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    id: u64,
) -> Result<bool, RocksError> {
    let key = primary(id);
    let Some(bytes) = transaction.get(NAMESPACE, key.as_bytes(), budget)? else {
        return Ok(false);
    };
    let record: Record = decode(&bytes)?;
    validate_record(&record)?;
    if record.id != id {
        return Err(corrupt());
    }
    if indexed(record.kind, record.state) {
        let key = scope_key(record.kind, record.scope, &record.scope_key);
        let value = transaction
            .get(NAMESPACE, key.as_bytes(), budget)?
            .ok_or(corrupt())?;
        if counter(&value)? != id {
            return Err(corrupt());
        }
        transaction.delete(NAMESPACE, key.as_bytes())?;
    }
    transaction.delete(NAMESPACE, key.as_bytes())?;
    crate::native_database::retention::forget_orphan(transaction, "artifact", key.as_bytes())?;
    transaction.delete(NAMESPACE, format!("u/{}", record.artifact_id).as_bytes())?;
    if record.kind == Kind::Summary {
        bump_epoch(transaction, budget, b"summary_epoch")?;
    }
    Ok(true)
}

pub(crate) fn bump_epoch(
    transaction: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    key: &[u8],
) -> Result<(), RocksError> {
    let value = transaction
        .get(StateNamespace::Metadata, key, budget)?
        .map(|bytes| counter(&bytes))
        .transpose()?
        .unwrap_or(0);
    transaction.put(
        StateNamespace::Metadata,
        key,
        value
            .checked_add(1)
            .ok_or(corrupt())?
            .to_be_bytes()
            .to_vec(),
    )
}

fn validate_record(record: &Record) -> Result<(), RocksError> {
    if record.id == 0
        || record.id > i64::MAX as u64
        || !canonical_uuid(&record.artifact_id)
        || validate_scope_key(&record.scope_key).is_err()
        || validate_body(&record.body, record.state).is_err()
        || validate_metadata(&record.metadata).is_err()
        || record
            .generation_id
            .as_ref()
            .is_some_and(|value| GenerationId::parse(value).is_err())
        || record
            .source_digest
            .as_ref()
            .is_some_and(|value| ContentDigest::parse(value).is_err())
        || record.created_at.parse::<jiff::Timestamp>().is_err()
        || record.updated_at.parse::<jiff::Timestamp>().is_err()
    {
        return Err(corrupt());
    }
    Ok(())
}

pub(crate) fn is_current(
    record: &Record,
    current: Option<&GenerationId>,
    graph: Option<&GraphSnapshot<'_, '_>>,
    budget: &mut ReadBudget,
) -> Result<bool, RocksError> {
    if matches!(record.kind, Kind::Role | Kind::Summary) && record.scope == Scope::Symbol {
        let Some(graph) = graph else {
            return Ok(false);
        };
        let id = SymbolId::parse(&record.scope_key).map_err(|_| corrupt())?;
        return Ok(graph.symbol(&id, budget)?.is_some_and(|symbol| {
            record.source_digest.as_deref() == Some(symbol.structural_digest.as_str())
        }));
    }
    Ok(
        current.is_some_and(|id| record.generation_id.as_deref() == Some(id.as_str()))
            && (!matches!(record.kind, Kind::Role | Kind::Summary)
                || (record.kind == Kind::Summary
                    && matches!(record.scope, Scope::File | Scope::Module | Scope::Project)
                    && record.source_digest.is_some())),
    )
}

fn newest(a: &Record, b: &Record) -> std::cmp::Ordering {
    timestamp_order(&b.updated_at, &a.updated_at).then(b.id.cmp(&a.id))
}
fn timestamp_ms(value: &str) -> Option<f64> {
    value
        .parse::<jiff::Timestamp>()
        .ok()?
        .as_millisecond()
        .to_f64()
}
fn counter(bytes: &[u8]) -> Result<u64, RocksError> {
    Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| corrupt())?))
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "native-artifact",
    }
}
fn workspace() -> RocksError {
    RocksError::Limit {
        resource: "artifact-workspace",
    }
}
