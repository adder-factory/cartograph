//! Model and vector services on the existing native project owner.

pub(crate) mod cache;
mod maintenance;
pub(crate) mod materialize;
mod neighbors;
pub(crate) mod vectors;

use super::types::{
    EmbeddingModelRegistration, EmbeddingModelSelector, EmbeddingModelState,
    RegisteredEmbeddingModel, SemanticStorageError, validate_timeout,
};
use crate::{
    NativeDatabase, StorageError,
    native_database::{decode, encode, visit_state},
    rocks::{ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use std::time::Duration;

const NAMESPACE: StateNamespace = StateNamespace::Embeddings;

/// Test exact source-compatible vectors without retaining the corpus's vector payloads.
pub(crate) fn embedded_symbols(
    transaction: &StateTransaction<'_>,
    graph: &crate::rocks::GraphSnapshot<'_, '_>,
    model_id: &cartograph_domain::ModelId,
    budget: &mut ReadBudget,
) -> Result<std::collections::BTreeSet<cartograph_domain::SymbolId>, SemanticStorageError> {
    let bytes = transaction
        .get(NAMESPACE, format!("m/{model_id}").as_bytes(), budget)?
        .ok_or(SemanticStorageError::ModelNotFound)?;
    let model: RegisteredEmbeddingModel = decode(&bytes)?;
    if model.selector.model_id != *model_id {
        return Err(SemanticStorageError::ModelMismatch);
    }
    vectors::active(transaction, &model.selector, budget)?;
    let generation = vectors::current(transaction)?;
    let mut output = std::collections::BTreeSet::new();
    vectors::documents_scan(graph, budget, |_, document, budget| {
        if document.kind == cartograph_domain::DocumentKind::Symbol
            && let Some(id) = document.symbol_id.clone()
        {
            let document_id = document.document_id.clone();
            let (_, digest) = vectors::source(transaction, graph, &generation, document, budget)?;
            if vectors::read_vector(transaction, &model.selector, &document_id, &digest, budget)?
                .is_some()
            {
                output.insert(id);
                if output.len() > 131_072 {
                    return Err(SemanticStorageError::InvalidInput {
                        field: "embedding_symbol_workspace",
                    });
                }
            }
        }
        Ok(std::ops::ControlFlow::Continue(()))
    })?;
    Ok(output)
}

pub(crate) fn feature_error(error: SemanticStorageError) -> StorageError {
    match error {
        SemanticStorageError::InvalidInput { field } => StorageError::InvalidInput { field },
        SemanticStorageError::CorruptStoredValue { field } => {
            StorageError::CorruptStoredValue { field }
        }
        SemanticStorageError::CurrentGenerationChanged
        | SemanticStorageError::SourceDigestChanged => StorageError::CurrentGenerationChanged,
        SemanticStorageError::CurrentGenerationUnavailable => StorageError::GenerationNotFound,
        SemanticStorageError::DatabaseOperation { operation } => {
            StorageError::DatabaseOperation { operation }
        }
        _ => StorageError::InvalidInput {
            field: "embedding_model",
        },
    }
}

impl NativeDatabase {
    /// Read one exact registered model without probing a provider endpoint.
    /// # Errors
    /// Rejects a missing or mismatched model and bounded native read failures.
    pub async fn registered_embedding_model(
        &self,
        selector: &EmbeddingModelSelector,
        timeout: Duration,
    ) -> Result<RegisteredEmbeddingModel, SemanticStorageError> {
        validate_timeout(timeout)?;
        let selector = selector.clone();
        self.transaction(None, 0, 0, timeout, move |transaction, budget| {
            load_model(transaction, &selector, budget)
        })
        .await
    }
    /// Register one immutable non-secret model identity in this project database.
    /// # Errors
    /// Rejects conflicting fingerprints, invalid deadlines, or corrupt model metadata.
    pub async fn register_embedding_model(
        &self,
        registration: EmbeddingModelRegistration,
        timeout: Duration,
    ) -> Result<RegisteredEmbeddingModel, SemanticStorageError> {
        validate_timeout(timeout)?;
        self.transaction(None, 1_048_576, 2, timeout, move |transaction, budget| {
            let proposed = RegisteredEmbeddingModel {
                selector: registration.selector(),
                provider: registration.provider().to_owned(),
                model_name: registration.model_name().to_owned(),
                normalization: registration.normalization(),
                state: EmbeddingModelState::Active,
            };
            let mut present = None;
            let mut count = 0_u16;
            visit_state::<RegisteredEmbeddingModel>(
                transaction,
                NAMESPACE,
                b"m/",
                budget,
                |_, model, _| {
                    count = count
                        .checked_add(1)
                        .ok_or(crate::rocks::RocksError::Limit {
                            resource: "embedding_models",
                        })?;
                    if model.selector.model_id == proposed.selector.model_id
                        || model.selector.fingerprint == proposed.selector.fingerprint
                    {
                        present = Some(model);
                    }
                    Ok(())
                },
            )?;
            if let Some(model) = present {
                if model.selector != proposed.selector
                    || model.provider != proposed.provider
                    || model.model_name != proposed.model_name
                    || model.normalization != proposed.normalization
                {
                    return Err(SemanticStorageError::ModelConflict);
                }
                return Ok(model);
            }
            if count >= 256 {
                return Err(SemanticStorageError::InvalidInput {
                    field: "embedding_models",
                });
            }
            crate::artifacts::native::bump_epoch(transaction, budget, b"vector_epoch")?;
            transaction.put(
                NAMESPACE,
                &model_key(&proposed.selector),
                encode(&proposed)?,
            )?;
            Ok(proposed)
        })
        .await
    }
}

fn model_key(model: &EmbeddingModelSelector) -> Vec<u8> {
    format!("m/{}", model.model_id.as_str()).into_bytes()
}

fn load_model(
    transaction: &StateTransaction<'_>,
    model: &EmbeddingModelSelector,
    budget: &mut ReadBudget,
) -> Result<RegisteredEmbeddingModel, SemanticStorageError> {
    let expected = model.clone();
    let model: RegisteredEmbeddingModel = transaction
        .get(NAMESPACE, &model_key(model), budget)?
        .map(|bytes| decode(&bytes))
        .transpose()?
        .ok_or(SemanticStorageError::ModelNotFound)?;
    if model.selector != expected {
        return Err(SemanticStorageError::ModelMismatch);
    }
    Ok(model)
}

impl From<RocksError> for SemanticStorageError {
    fn from(error: RocksError) -> Self {
        Self::from(crate::native_database::storage_error(error))
    }
}

impl From<StorageError> for SemanticStorageError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::InvalidInput { field } => Self::InvalidInput { field },
            StorageError::CurrentGenerationChanged => Self::CurrentGenerationChanged,
            StorageError::GenerationNotFound => Self::CurrentGenerationUnavailable,
            StorageError::ImmutableConflict => Self::ModelConflict,
            StorageError::CorruptStoredValue { field } => Self::CorruptStoredValue { field },
            _ => Self::DatabaseOperation {
                operation: "native-semantic",
            },
        }
    }
}
