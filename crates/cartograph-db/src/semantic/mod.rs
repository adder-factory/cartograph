mod maintenance;
mod materialize;
pub(crate) mod native;
mod rerank;
mod types;

pub use maintenance::{EmbeddingStorageAudit, RetiredEmbeddingCleanupReport};
pub use materialize::{SimilarityMaterializationPolicy, SimilarityMaterializationReport};
pub use rerank::{RerankDocument, RerankDocumentsRequest};
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
