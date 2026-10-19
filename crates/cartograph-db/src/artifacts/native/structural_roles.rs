//! Metadata-only rules stream native symbol pages without rescanning source documents.

use super::{
    Kind, Record, Scope, State, WRITE_BYTES, WRITE_ROWS, corrupt, read_scope, workspace, write,
};
use crate::{
    NativeDatabase, NewAgentArtifact, StorageError,
    artifacts::{
        PendingRoleSymbol, StructuralRoleCursor, StructuralRolePage, SymbolRoleSaveInput,
        validate_body, validate_limit, validate_metadata, validate_model,
    },
    rocks::{ReadBudget, RocksError, StateTransaction},
};
use cartograph_domain::{GenerationId, ProjectId, SymbolId};

struct OwnedRole {
    id: SymbolId,
    body: String,
    metadata: serde_json::Value,
}
enum RoleSave {
    Existing(Record),
    New(NewAgentArtifact),
}

fn owned_roles<'a>(
    database: &NativeDatabase,
    expected: Option<&GenerationId>,
    inputs: impl IntoIterator<Item = SymbolRoleSaveInput<'a>>,
) -> Result<Vec<OwnedRole>, StorageError> {
    let mut owned = Vec::<OwnedRole>::new();
    let mut bytes = 0_usize;
    for input in inputs {
        database.require_project(input.project_id)?;
        validate_body(input.role, State::Complete)?;
        validate_metadata(&input.metadata)?;
        let metadata_bytes = serde_json::to_vec(&input.metadata)
            .map_err(|_| StorageError::InvalidInput {
                field: "artifact_metadata",
            })?
            .len();
        bytes = bytes
            .checked_add(input.role.len() + metadata_bytes + 1024)
            .ok_or(StorageError::InvalidInput {
                field: "role_batch",
            })?;
        if owned.len() == 16
            || bytes > WRITE_BYTES as usize
            || owned.iter().any(|item| &item.id == input.symbol_id)
            || input
                .generation_id
                .is_some_and(|generation| Some(generation) != expected)
        {
            return Err(StorageError::InvalidInput {
                field: "role_batch",
            });
        }
        owned.push(OwnedRole {
            id: input.symbol_id.clone(),
            body: input.role.to_owned(),
            metadata: input.metadata,
        });
    }
    if owned.is_empty() {
        return Err(StorageError::InvalidInput {
            field: "role_batch",
        });
    }
    Ok(owned)
}

fn prepare_role(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    role: OwnedRole,
    preserve_judged: bool,
) -> Result<RoleSave, RocksError> {
    let input = super::summaries::symbol_input(
        transaction,
        budget,
        &role.id,
        None,
        Kind::Role,
        role.body,
        role.metadata,
    )?
    .ok_or(RocksError::GenerationChanged)?;
    if preserve_judged && let Some(record) = preserve_judged_role(transaction, budget, &input)? {
        return Ok(RoleSave::Existing(record));
    }
    Ok(RoleSave::New(input))
}

pub(super) fn preserve_judged_role(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    input: &NewAgentArtifact,
) -> Result<Option<Record>, RocksError> {
    if !matches!(
        input
            .metadata
            .get("via")
            .and_then(serde_json::Value::as_str),
        Some("structural_rule" | "structural_fallback")
    ) {
        return Ok(None);
    }
    Ok(read_scope(
        transaction,
        Kind::Role,
        Scope::Symbol,
        &input.scope_key,
        budget,
    )?
    .filter(|record| {
        record.state == State::Complete
            && record.source_digest.as_deref()
                == input
                    .source_digest
                    .as_ref()
                    .map(cartograph_domain::ContentDigest::as_str)
            && matches!(
                record
                    .metadata
                    .get("via")
                    .and_then(serde_json::Value::as_str),
                Some("llm" | "jev")
            )
    }))
}

impl NativeDatabase {
    /// Atomically save at most sixteen roles under one current-generation fence.
    /// Structural producers preserve a digest-compatible current judged role.
    /// # Errors
    /// Rejects mixed projects/generations, duplicate symbols, more than 1 MiB input,
    /// invalid roles, absent source, changed generation or bounded native write failure.
    pub async fn save_symbol_roles<'a>(
        &self,
        generation: &GenerationId,
        inputs: impl IntoIterator<Item = SymbolRoleSaveInput<'a>>,
    ) -> Result<Vec<Record>, StorageError> {
        self.save_role_batch(Some(generation.clone()), inputs).await
    }

    pub(super) async fn save_role_batch<'a>(
        &self,
        generation: Option<GenerationId>,
        inputs: impl IntoIterator<Item = SymbolRoleSaveInput<'a>>,
    ) -> Result<Vec<Record>, StorageError> {
        let roles = owned_roles(self, generation.as_ref(), inputs)?;
        let rows = u32::try_from(roles.len()).map_err(|_| StorageError::InvalidInput {
            field: "role_batch",
        })? * WRITE_ROWS;
        let preserve_judged = generation.is_some();
        self.state(generation, WRITE_BYTES, rows, move |transaction, budget| {
            let prepared = roles
                .into_iter()
                .map(|role| prepare_role(transaction, budget, role, preserve_judged))
                .collect::<Result<Vec<_>, _>>()?;
            prepared
                .into_iter()
                .map(|role| match role {
                    RoleSave::Existing(record) => Ok(record),
                    RoleSave::New(input) => write(transaction, budget, input, true),
                })
                .collect()
        })
        .await
    }

    /// Scan at most `limit` declarations after a generation/model-pinned native cursor.
    /// Rules use metadata only; judged producers retain the richer priority-ordered backlog API.
    /// # Errors
    /// Rejects invalid bounds/model, a changed generation, corrupt source or exhausted budgets.
    pub async fn pending_structural_roles(
        &self,
        project: &ProjectId,
        model: &str,
        after: Option<&StructuralRoleCursor>,
        limit: u16,
    ) -> Result<StructuralRolePage, StorageError> {
        self.require_project(project)?;
        validate_limit(limit, super::super::MAX_ROLE_BATCH)?;
        validate_model(model)?;
        if after.is_some_and(|cursor| cursor.model != model) {
            return Err(StorageError::InvalidInput {
                field: "role_cursor",
            });
        }
        let (model, after) = (model.to_owned(), after.cloned());
        self.state(
            after.as_ref().map(|cursor| cursor.generation.clone()),
            0,
            0,
            move |transaction, budget| {
                let generation_id = transaction
                    .current()?
                    .ok_or(RocksError::GenerationChanged)?
                    .generation_id;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let (rows, next) =
                    graph.symbols(after.as_ref().map(|cursor| &cursor.after), limit, budget)?;
                let mut symbols = Vec::new();
                let mut bytes = 0_usize;
                for symbol in rows {
                    if matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter") {
                        continue;
                    }
                    let role = read_scope(
                        transaction,
                        Kind::Role,
                        Scope::Symbol,
                        symbol.symbol_id.as_str(),
                        budget,
                    )?;
                    if role.is_some_and(|record| {
                        super::backlog::role_satisfied(&record, &symbol, &model, true)
                    }) {
                        continue;
                    }
                    let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                    bytes = bytes
                        .checked_add(
                            symbol.qualified_name.len()
                                + file.normalized_path.len()
                                + file.language.len()
                                + 8192,
                        )
                        .ok_or_else(workspace)?;
                    if bytes > 16 * 1_048_576 {
                        return Err(workspace());
                    }
                    symbols.push(PendingRoleSymbol {
                        symbol_id: symbol.symbol_id.to_string(),
                        path: file.normalized_path,
                        language: file.language,
                        symbol_kind: symbol.symbol_kind,
                        qualified_name: symbol.qualified_name,
                        signature: symbol.signature.chars().take(1024).collect(),
                        description: String::new(),
                        code: String::new(),
                        exported: symbol.export.exported,
                    });
                }
                let next = next.map(|after| StructuralRoleCursor {
                    generation: generation_id.clone(),
                    model,
                    after,
                });
                Ok(StructuralRolePage {
                    generation_id,
                    symbols,
                    next,
                })
            },
        )
        .await
    }
}
