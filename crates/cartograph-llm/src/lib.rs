//! Bounded, secret-safe OpenAI-compatible embedding client for Cartograph v2.

mod chat;
mod client;
mod config;
mod endpoint;
mod jev;
mod model;
mod models;
mod project_config;
mod rerank;
mod tls;
mod transport;

pub use chat::{
    CHAT_API_KEY_ENV, CHAT_ENDPOINT_ENV, CHAT_MODEL_ENV, ChatCompletion, ChatError,
    ChatMessageRequest, ChatSettings, GroundedChatRequest, OpenAiChatClient,
};
pub use client::OpenAiEmbeddingClient;
pub use config::{
    EMBEDDING_API_KEY_ENV, EMBEDDING_ENDPOINT_ENV, EMBEDDING_MODEL_ENV, EmbeddingSettings,
};
pub use jev::{
    JEV_ENDPOINT, JEV_MODEL, JevAnswer, JevClient, JevDecision, JevError, JevQuestion, JevSettings,
    NoulCriteria,
};
pub use model::{EmbeddingBatch, EmbeddingError, EmbeddingModelIdentity, EmbeddingVector};
pub use models::{
    InstallModelsError, InstallModelsOptions, InstallModelsReport, InstalledModel,
    install_recommended_models,
};
pub use project_config::{
    CliBridgeConfig, CliBridgeConfigInput, CliBridgeInputMode, CliBridgeResponseFormat,
    LlmEndpointProbe, ProjectCredentialMigrationEntry, ProjectCredentialMigrationReport,
    ProjectCredentialMigrationStatus, ProjectGenerationStorage, ProjectLlmConfigError,
    ProjectLlmCredentialSource, ProjectLlmCredentialWriteAction, ProjectLlmCredentialWriteEntry,
    ProjectLlmProvider, ProjectLlmTier, ProjectLlmTierConfig, ProjectLlmTierInput,
    ProjectLlmWriteReport, ProjectSourceSettings, ProjectSummaryEagerLimit, ProjectSummarySettings,
    load_exact_project_llm_tier, load_project_llm_tier, load_project_max_file_size,
    load_project_source_settings, load_project_summary_settings,
    migrate_project_inline_credentials, probe_openai_compatible_endpoint, tune_project_llm_tier,
    write_project_llm_configuration, write_project_llm_configuration_with_report,
    write_project_llm_tiers, write_project_max_file_size,
};
pub use rerank::{OpenAiRerankClient, RerankBatch, RerankError, RerankSettings};
pub use tls::{TlsProviderUnavailable, ensure_tls_crypto_provider};
