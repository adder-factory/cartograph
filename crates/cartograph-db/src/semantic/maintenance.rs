use serde::Serialize;

/// Database-only semantic storage evidence; no endpoint or credential is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingStorageAudit {
    /// Number of active models.
    pub active_models: u64,
    /// Number of retired models.
    pub retired_models: u64,
    /// Number of current documents.
    pub current_documents: u64,
    /// Number of current embeddings.
    pub current_embeddings: u64,
    /// Number of historical embeddings.
    pub historical_embeddings: u64,
    /// Number of retired model embeddings.
    pub retired_model_embeddings: u64,
    /// Number of model indexes.
    pub model_indexes: u64,
}

/// Dry-run or applied cleanup of embeddings belonging to retired models.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetiredEmbeddingCleanupReport {
    /// Whether the operation reports changes without applying them.
    pub dry_run: bool,
    /// Candidate embeddings observed in this bounded batch; not a full census.
    pub candidate_embeddings: u64,
    /// Number of deleted embeddings.
    pub deleted_embeddings: u64,
    /// Number of dropped model indexes.
    pub dropped_model_indexes: u64,
    /// Additional candidates may remain beyond this committed batch.
    pub incomplete: bool,
}
