mod maintenance;
mod materialize;
pub(crate) mod native;
mod types;

pub use maintenance::{EmbeddingStorageAudit, RetiredEmbeddingCleanupReport};
pub use materialize::{SimilarityMaterializationPolicy, SimilarityMaterializationReport};
pub use types::{
    EmbeddingBatchUpsertInput, EmbeddingBatchUpsertReport, EmbeddingBatchUpsertRequest,
    EmbeddingIndexStatus, EmbeddingModelRegistration, EmbeddingModelRegistrationInput,
    EmbeddingModelSelector, EmbeddingModelState, EmbeddingNormalization, EmbeddingPageCursor,
    EmbeddingUpsertRow, PendingEmbeddingDocument, PendingEmbeddingPage, PendingEmbeddingPageInput,
    PendingEmbeddingPageRequest, RegisteredEmbeddingModel, RetireEmbeddingModelRequest,
    SemanticReadinessReport, SemanticReadinessRequest, SemanticReadinessState,
    SemanticStorageError, SimilarSymbolHit, SimilarSymbolsInput, SimilarSymbolsRequest,
    SimilarSymbolsResult, VectorSearchHit, VectorSearchInput, VectorSearchRequest,
};

mod common;
