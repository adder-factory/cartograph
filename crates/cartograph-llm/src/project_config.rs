use cartograph_config::{BoundedU64Field, optional_bounded_u64};
use std::{collections::BTreeMap, env, path::Path};

use futures_util::StreamExt as _;
use num_traits::ToPrimitive as _;
use reqwest::StatusCode;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use url::Url;

pub use cartograph_config::{ProjectGenerationStorage, ProjectSourceSettings};

const MAXIMUM_MODEL_BYTES: usize = 256;
const MAXIMUM_API_KEY_BYTES: usize = 8_192;
const MAXIMUM_TIMEOUT_MS: u64 = 600_000;
const MAXIMUM_CONCURRENCY: u16 = 16;
const MAXIMUM_SUMMARY_BATCH_SIZE: u16 = 16;
const MAXIMUM_CLI_COMMAND_BYTES: usize = 4_096;
const MAXIMUM_CLI_ARGUMENTS: usize = 128;
const MAXIMUM_CLI_ARGUMENT_BYTES: usize = 4_096;
const MAXIMUM_CLI_ARGUMENT_TOTAL_BYTES: usize = 32 * 1_024;
const MAXIMUM_CLI_PROMPT_TEMPLATE_BYTES: usize = 64 * 1_024;
const MAXIMUM_CLI_RESPONSE_PATH_BYTES: usize = 4_096;
const MAXIMUM_CLI_RESPONSE_PATH_COMPONENTS: usize = 64;
const MAXIMUM_LLAMA_SERVER_ARGUMENTS: usize = 128;
const MAXIMUM_LLAMA_SERVER_ARGUMENT_BYTES: usize = 4_096;
const MAXIMUM_LLAMA_SERVER_ARGUMENT_TOTAL_BYTES: usize = 32 * 1_024;
const MAXIMUM_PROBE_BYTES: usize = 1024 * 1024;
const MAXIMUM_PROBE_MODELS: usize = 128;
const OPENAI_CLOUD_ENDPOINT: &str = "https://api.openai.com";
const ANTHROPIC_CLOUD_ENDPOINT: &str = "https://api.anthropic.com";
const CLAUDE_BRIDGE_ENDPOINT: &str = "claude-bridge://local";
const CLI_BRIDGE_ENDPOINT: &str = "cli-bridge://local";
const DEFAULT_CLI_PROMPT_TEMPLATE: &str = "# System\n{system}\n\n# User\n{user}";
const DEFAULT_CLAUDE_SUMMARIZE_MODEL: &str = "claude-haiku-4-5";
const DEFAULT_CLAUDE_ASK_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_SUMMARY_EAGER_LIMIT: u64 = 600;
const MAXIMUM_SUMMARY_EAGER_LIMIT: u64 = 10_000_000;
const DEFAULT_SUMMARY_MINIMUM_BODY_LINES: u32 = 4;
const MAXIMUM_SUMMARY_MINIMUM_BODY_LINES: u32 = 1_000_000;
const MAXIMUM_SUMMARY_KIND_OVERRIDES: usize = 128;
const MAXIMUM_SUMMARY_KIND_BYTES: usize = 64;

/// LLM slots preserved from the v1.1.33 project-config contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLlmTier {
    /// Represents the embedding project LLM tier.
    Embedding,
    /// Represents the summarize project LLM tier.
    Summarize,
    /// Represents the local project LLM tier.
    Local,
    /// Represents the ask project LLM tier.
    Ask,
    /// Represents the classify project LLM tier.
    Classify,
    /// Represents the reranker project LLM tier.
    Reranker,
    /// Optional Jev decisions for bounded retrieval planning.
    Decision,
}

/// Validated provider retained from the v1.1.33 chat configuration contract.
/// Embedding and reranker tiers remain OpenAI-compatible HTTP only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectLlmProvider {
    /// Represents the open ai compat project LLM provider.
    OpenAiCompat,
    /// Represents the claude bridge project LLM provider.
    ClaudeBridge,
    /// Represents a bounded provider-agnostic local CLI bridge.
    CliBridge,
    /// Represents the anthropic API project LLM provider.
    AnthropicApi,
    /// Typesafe's typed, parallel Jev decision API.
    Typesafe,
}

/// Effective project-wide eager summary budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSummaryEagerLimit {
    /// Represents the bounded project summary eager limit.
    Bounded(u64),
    /// Represents the uncapped project summary eager limit.
    Uncapped,
}

/// V1-compatible summary candidate and eager-run settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSummarySettings {
    enabled: bool,
    eager_limit: ProjectSummaryEagerLimit,
    minimum_body_lines: u32,
    minimum_body_lines_by_kind: BTreeMap<String, u32>,
}

impl ProjectSummarySettings {
    #[must_use]
    /// Returns whether summary generation is enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    /// Returns the eager limit.
    pub const fn eager_limit(&self) -> ProjectSummaryEagerLimit {
        self.eager_limit
    }

    #[must_use]
    /// Returns the minimum body lines.
    pub const fn minimum_body_lines(&self) -> u32 {
        self.minimum_body_lines
    }

    #[must_use]
    /// Returns the minimum body lines by kind.
    pub const fn minimum_body_lines_by_kind(&self) -> &BTreeMap<String, u32> {
        &self.minimum_body_lines_by_kind
    }
}

impl ProjectLlmProvider {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "openai-compat" => Some(Self::OpenAiCompat),
            "claude-bridge" => Some(Self::ClaudeBridge),
            "cli-bridge" => Some(Self::CliBridge),
            "anthropic-api" => Some(Self::AnthropicApi),
            "typesafe" => Some(Self::Typesafe),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompat => "openai-compat",
            Self::ClaudeBridge => "claude-bridge",
            Self::CliBridge => "cli-bridge",
            Self::AnthropicApi => "anthropic-api",
            Self::Typesafe => "typesafe",
        }
    }
}

/// How a generic CLI bridge receives its rendered prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliBridgeInputMode {
    /// Write the prompt to the child process stdin and close it before waiting.
    Stdin,
    /// Substitute the prompt into one exact argv entry without a shell.
    Arg,
}

impl CliBridgeInputMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "stdin" => Some(Self::Stdin),
            "arg" => Some(Self::Arg),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Stdin => "stdin",
            Self::Arg => "arg",
        }
    }
}

/// Decoder applied to bounded stdout from a generic CLI bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliBridgeResponseFormat {
    /// Treat trimmed UTF-8 stdout as the completion.
    Raw,
    /// Extract a string from bounded JSON stdout through a validated path.
    JsonPath,
    /// Decode the historical Claude CLI JSON envelope.
    Claude,
}

impl CliBridgeResponseFormat {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "raw" => Some(Self::Raw),
            "json-path" => Some(Self::JsonPath),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::JsonPath => "json-path",
            Self::Claude => "claude",
        }
    }
}

/// Inputs for one validated shell-free CLI bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliBridgeConfigInput {
    command: String,
    args: Vec<String>,
    input: CliBridgeInputMode,
    response_format: CliBridgeResponseFormat,
    response_path: Option<String>,
}

impl CliBridgeConfigInput {
    /// Start a CLI bridge contract with no argv or response path.
    pub fn new(
        command: impl Into<String>,
        input: CliBridgeInputMode,
        response_format: CliBridgeResponseFormat,
    ) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            input,
            response_format,
            response_path: None,
        }
    }

    /// Supply the exact ordered argv templates passed without a shell.
    #[must_use]
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Supply the validated JSON path used by the JSON-path decoder.
    #[must_use]
    pub fn with_response_path(mut self, response_path: Option<String>) -> Self {
        self.response_path = response_path;
        self
    }
}

/// Validated shell-free command contract for one chat-family CLI bridge.
#[derive(Clone, PartialEq, Eq)]
pub struct CliBridgeConfig {
    command: String,
    args: Vec<String>,
    input: CliBridgeInputMode,
    prompt_template: String,
    response_format: CliBridgeResponseFormat,
    response_path: Option<String>,
}

impl std::fmt::Debug for CliBridgeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CliBridgeConfig")
            .field("command", &"<configured>")
            .field("argument_count", &self.args.len())
            .field("input", &self.input)
            .field("prompt_template", &"<configured>")
            .field("response_format", &self.response_format)
            .field("response_path_configured", &self.response_path.is_some())
            .finish()
    }
}

impl CliBridgeConfig {
    /// Build one bounded argv-only CLI bridge contract.
    /// # Errors
    ///
    /// Returns an error for unsafe command/argument text, invalid substitution
    /// tokens, an input-mode mismatch, or an invalid response decoder/path pair.
    pub fn new(input: CliBridgeConfigInput) -> Result<Self, ProjectLlmConfigError> {
        let config = Self {
            command: input.command,
            args: input.args,
            input: input.input,
            prompt_template: DEFAULT_CLI_PROMPT_TEMPLATE.to_owned(),
            response_format: input.response_format,
            response_path: input.response_path,
        };
        validate_cli_bridge_config(&config)?;
        Ok(config)
    }

    /// Replace the default system/user prompt template.
    /// # Errors
    ///
    /// Returns an error unless the template is bounded and contains only the
    /// required `{system}` and `{user}` substitution tokens.
    pub fn with_prompt_template(
        mut self,
        prompt_template: impl Into<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        self.prompt_template = prompt_template.into();
        validate_cli_bridge_config(&self)?;
        Ok(self)
    }

    /// Build the historical Claude bridge argv, stdin, prompt, and decoder contract.
    /// # Errors
    ///
    /// Returns an error when an explicit command violates the CLI command bound.
    pub fn claude_compatible(command: Option<&str>) -> Result<Self, ProjectLlmConfigError> {
        Self::new(
            CliBridgeConfigInput::new(
                command.unwrap_or("claude"),
                CliBridgeInputMode::Stdin,
                CliBridgeResponseFormat::Claude,
            )
            .with_args(
                [
                    "-p",
                    "--strict-mcp-config",
                    "--no-session-persistence",
                    "--disable-slash-commands",
                    "--model",
                    "{model}",
                    "--output-format",
                    "json",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            ),
        )
    }

    /// Exact executable passed directly to `Command::new`.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Exact argv templates passed without shell interpolation.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Configured prompt delivery mode.
    #[must_use]
    pub const fn input(&self) -> CliBridgeInputMode {
        self.input
    }

    /// Bounded template rendered from trusted system and user strings.
    #[must_use]
    pub fn prompt_template(&self) -> &str {
        &self.prompt_template
    }

    /// Configured bounded stdout decoder.
    #[must_use]
    pub const fn response_format(&self) -> CliBridgeResponseFormat {
        self.response_format
    }

    /// Validated JSON path used only by the JSON-path decoder.
    #[must_use]
    pub fn response_path(&self) -> Option<&str> {
        self.response_path.as_deref()
    }
}

impl ProjectLlmTier {
    const fn config_key(self) -> &'static str {
        match self {
            Self::Embedding => "embeddingLlm",
            Self::Summarize => "summarizeLlm",
            Self::Local => "localLlm",
            Self::Ask => "askLlm",
            Self::Classify => "classifyLlm",
            Self::Reranker => "rerankerLlm",
            Self::Decision => "decisionLlm",
        }
    }

    const fn fallback(self) -> Option<Self> {
        match self {
            Self::Local | Self::Ask | Self::Classify => Some(Self::Summarize),
            Self::Embedding | Self::Summarize | Self::Reranker | Self::Decision => None,
        }
    }
}

/// Where a configured Bearer credential is resolved without exposing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLlmCredentialSource {
    /// Represents the none project LLM credential source.
    None,
    /// Represents the environment project LLM credential source.
    Environment,
    /// Represents the inline legacy project LLM credential source.
    InlineLegacy,
}

/// Secret-free outcome for one configured tier credential mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLlmCredentialWriteAction {
    /// No credential existed and no credential mutation was needed.
    Unchanged,
    /// An existing credential was retained because the provider origin did not change.
    Preserved,
    /// The caller explicitly removed every credential reference.
    ClearedExplicitly,
    /// Cartograph removed credentials because the provider endpoint origin changed.
    ClearedOriginChange,
    /// The caller replaced any previous credential with an environment reference.
    EnvironmentReferenceSet,
}

/// One tier's secret-free credential mutation result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLlmCredentialWriteEntry {
    /// Tier whose configuration was updated.
    pub tier: ProjectLlmTier,
    /// Credential action applied without exposing credential material.
    pub action: ProjectLlmCredentialWriteAction,
}

/// Atomic project LLM configuration write report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLlmWriteReport {
    /// One entry for every configured tier mutation.
    pub credential_actions: Vec<ProjectLlmCredentialWriteEntry>,
}

/// Secret-safe outcome for one legacy inline credential migration candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCredentialMigrationStatus {
    /// Represents the ready credential migration status state.
    Ready,
    /// Represents the migrated credential migration status state.
    Migrated,
    /// Represents the environment missing credential migration status state.
    EnvironmentMissing,
    /// Represents the environment mismatch credential migration status state.
    EnvironmentMismatch,
    /// Represents the unsupported provider credential migration status state.
    UnsupportedProvider,
}

/// One credential migration decision. The credential value is never retained
/// in or exposed by this report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCredentialMigrationEntry {
    /// Tier for this record.
    pub tier: ProjectLlmTier,
    /// Optional environment, when available.
    pub environment: Option<String>,
    /// Status for this record.
    pub status: ProjectCredentialMigrationStatus,
}

/// Atomic project credential migration report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCredentialMigrationReport {
    /// Whether migration changes are reported without being written.
    pub dry_run: bool,
    /// Bounded candidates included in this result.
    pub candidates: Vec<ProjectCredentialMigrationEntry>,
    /// Number of migrated.
    pub migrated: usize,
    /// Number of remaining inline.
    pub remaining_inline: usize,
}

/// Bounded `/v1/models` probe used by the agent-driven setup planner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmEndpointProbe {
    /// Whether the configured endpoint accepted a bounded probe.
    pub reachable: bool,
    /// Whether the endpoint satisfied the OpenAI-compatible response contract.
    pub openai_compatible: bool,
    /// Bounded models included in this result.
    pub models: Vec<String>,
}

/// One validated OpenAI-compatible project tier.
#[derive(Clone)]
pub struct ProjectLlmTierConfig {
    provider: ProjectLlmProvider,
    endpoint: String,
    model: String,
    ask_model: Option<String>,
    api_key: Option<SecretString>,
    unavailable_credential_env: Option<String>,
    credential_source: ProjectLlmCredentialSource,
    timeout_ms: Option<u64>,
    concurrency: Option<u16>,
    summary_batch_size: Option<u16>,
    claude_bin: Option<String>,
    cli_bridge: Option<CliBridgeConfig>,
    llama_server_args: Vec<String>,
    externally_managed: bool,
    decision_features: Option<Vec<String>>,
}

impl std::fmt::Debug for ProjectLlmTierConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectLlmTierConfig")
            .field("provider", &self.provider)
            .field("endpoint", &"<redacted>")
            .field("model", &self.model)
            .field("ask_model", &self.ask_model)
            .field("api_key_configured", &self.api_key.is_some())
            .field(
                "unavailable_credential_env",
                &self.unavailable_credential_env,
            )
            .field("credential_source", &self.credential_source)
            .field("timeout_ms", &self.timeout_ms)
            .field("concurrency", &self.concurrency)
            .field("summary_batch_size", &self.summary_batch_size)
            .field("claude_binary_configured", &self.claude_bin.is_some())
            .field("cli_bridge_configured", &self.cli_bridge.is_some())
            .field("llama_server_argument_count", &self.llama_server_args.len())
            .field("externally_managed", &self.externally_managed)
            .field("decision_features", &self.decision_features)
            .finish()
    }
}

impl ProjectLlmTierConfig {
    #[must_use]
    /// Returns the provider.
    pub const fn provider(&self) -> ProjectLlmProvider {
        self.provider
    }

    #[must_use]
    /// Returns the endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    /// Returns the model.
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    /// Returns the ask model.
    pub fn ask_model(&self) -> Option<&str> {
        self.ask_model.as_deref()
    }

    #[must_use]
    /// Returns the API key.
    pub fn api_key(&self) -> Option<String> {
        self.api_key
            .as_ref()
            .map(|value| value.expose_secret().to_owned())
    }

    /// Validated environment variable name missing from this process, never its value.
    #[must_use]
    pub fn unavailable_credential_env(&self) -> Option<&str> {
        self.unavailable_credential_env.as_deref()
    }

    #[must_use]
    /// Returns the credential source.
    pub const fn credential_source(&self) -> ProjectLlmCredentialSource {
        self.credential_source
    }

    #[must_use]
    /// Returns the timeout milliseconds.
    pub const fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }

    #[must_use]
    /// Returns the concurrency.
    pub const fn concurrency(&self) -> Option<u16> {
        self.concurrency
    }

    #[must_use]
    /// Returns the summary batch size.
    pub const fn summary_batch_size(&self) -> Option<u16> {
        self.summary_batch_size
    }

    #[must_use]
    /// Returns the claude bin.
    pub fn claude_bin(&self) -> Option<&str> {
        self.claude_bin.as_deref()
    }

    /// Returns the validated generic CLI bridge contract, when configured.
    #[must_use]
    pub const fn cli_bridge(&self) -> Option<&CliBridgeConfig> {
        self.cli_bridge.as_ref()
    }

    #[must_use]
    /// Returns the llama server args.
    pub fn llama_server_args(&self) -> &[String] {
        &self.llama_server_args
    }

    #[must_use]
    /// Returns whether the configured backend is managed externally.
    pub const fn externally_managed(&self) -> bool {
        self.externally_managed
    }

    #[must_use]
    /// Decision-tier feature names the project allows to consult the provider;
    /// `None` when the tier does not list them, meaning exploration only.
    pub fn decision_features(&self) -> Option<&[String]> {
        self.decision_features.as_deref()
    }
}

#[derive(Clone, Debug)]
enum ProjectLlmCredentialIntent {
    Preserve,
    Clear,
    Environment(String),
}

/// Validated config mutation. Credentials are preserved only while the
/// provider endpoint origin is unchanged, unless explicitly cleared or
/// replaced with an environment-variable reference.
#[derive(Clone, Debug)]
pub struct ProjectLlmTierInput {
    tier: ProjectLlmTier,
    provider: ProjectLlmProvider,
    endpoint: String,
    model: String,
    ask_model: Option<String>,
    credential_intent: ProjectLlmCredentialIntent,
    timeout_ms: Option<u64>,
    concurrency: Option<u16>,
    summary_batch_size: Option<u16>,
    claude_bin: Option<String>,
    cli_bridge: Option<CliBridgeConfig>,
    decision_features: Option<Vec<String>>,
}

impl ProjectLlmTierInput {
    /// Creates a validated project LLM tier input.
    ///
    /// # Errors
    ///
    /// Returns an error if `endpoint` is unsafe/invalid or `model` is empty,
    /// oversized, or contains control characters.
    pub fn new(
        tier: ProjectLlmTier,
        endpoint: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        let endpoint = endpoint.into();
        let model = model.into();
        validate_endpoint(&endpoint)?;
        validate_model(&model)?;
        if tier == ProjectLlmTier::Decision {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        Ok(Self {
            tier,
            provider: ProjectLlmProvider::OpenAiCompat,
            endpoint,
            model,
            ask_model: None,
            credential_intent: ProjectLlmCredentialIntent::Preserve,
            timeout_ms: None,
            concurrency: None,
            summary_batch_size: None,
            claude_bin: None,
            cli_bridge: None,
            decision_features: None,
        })
    }

    /// Configure the optional Jev decision tier using an environment credential.
    /// # Errors
    ///
    /// Returns an error when the environment-variable name is invalid.
    pub fn jev(api_key_env: impl Into<String>) -> Result<Self, ProjectLlmConfigError> {
        let mut input = Self::new(
            ProjectLlmTier::Classify,
            crate::jev::JEV_ENDPOINT,
            crate::jev::JEV_MODEL,
        )?;
        input.tier = ProjectLlmTier::Decision;
        input.provider = ProjectLlmProvider::Typesafe;
        input.with_api_key_env(api_key_env)
    }

    /// Replace the decision tier's provider feature allowlist, for example
    /// `["explore", "context"]`. Other tiers reject a feature list.
    /// # Errors
    ///
    /// Returns an error for a non-decision tier or an invalid feature name.
    pub fn with_decision_features(
        mut self,
        features: Vec<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        if self.tier != ProjectLlmTier::Decision {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        let encoded = Value::Array(features.iter().cloned().map(Value::String).collect());
        let mut object = Map::new();
        object.insert("features".to_owned(), encoded);
        parse_decision_features(&object)?;
        self.decision_features = Some(features);
        Ok(self)
    }

    /// Returns the claude bridge.
    ///
    /// # Errors
    ///
    /// Returns an error if `tier` is not chat-capable or `model` violates its
    /// non-empty, byte-length, or control-character contract.
    pub fn claude_bridge(
        tier: ProjectLlmTier,
        model: impl Into<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        validate_chat_tier(tier)?;
        let model = model.into();
        validate_model(&model)?;
        Ok(Self {
            tier,
            provider: ProjectLlmProvider::ClaudeBridge,
            endpoint: CLAUDE_BRIDGE_ENDPOINT.to_owned(),
            model,
            ask_model: None,
            credential_intent: ProjectLlmCredentialIntent::Clear,
            timeout_ms: None,
            concurrency: None,
            summary_batch_size: None,
            claude_bin: None,
            cli_bridge: None,
            decision_features: None,
        })
    }

    /// Build a credential-free provider-agnostic CLI bridge for a chat tier.
    /// # Errors
    ///
    /// Returns an error if the tier is not chat-capable or the model is invalid.
    pub fn cli_bridge(
        tier: ProjectLlmTier,
        model: impl Into<String>,
        config: CliBridgeConfig,
    ) -> Result<Self, ProjectLlmConfigError> {
        validate_chat_tier(tier)?;
        let model = model.into();
        validate_model(&model)?;
        Ok(Self {
            tier,
            provider: ProjectLlmProvider::CliBridge,
            endpoint: CLI_BRIDGE_ENDPOINT.to_owned(),
            model,
            ask_model: None,
            credential_intent: ProjectLlmCredentialIntent::Clear,
            timeout_ms: None,
            concurrency: None,
            summary_batch_size: None,
            claude_bin: None,
            cli_bridge: Some(config),
            decision_features: None,
        })
    }

    /// Returns the anthropic API.
    ///
    /// # Errors
    ///
    /// Returns an error if `tier` is not chat-capable or `model` violates its
    /// non-empty, byte-length, or control-character contract.
    pub fn anthropic_api(
        tier: ProjectLlmTier,
        model: impl Into<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        validate_chat_tier(tier)?;
        let model = model.into();
        validate_model(&model)?;
        Ok(Self {
            tier,
            provider: ProjectLlmProvider::AnthropicApi,
            endpoint: ANTHROPIC_CLOUD_ENDPOINT.to_owned(),
            model,
            ask_model: None,
            credential_intent: ProjectLlmCredentialIntent::Environment(
                "ANTHROPIC_API_KEY".to_owned(),
            ),
            timeout_ms: None,
            concurrency: None,
            summary_batch_size: None,
            claude_bin: None,
            cli_bridge: None,
            decision_features: None,
        })
    }

    /// Tier selected by this validated configuration mutation.
    ///
    /// Setup and doctor workflows use this to apply only genuinely missing
    /// tiers without overwriting an operator's existing backend choices.
    #[must_use]
    pub const fn tier(&self) -> ProjectLlmTier {
        self.tier
    }

    /// Sets the API key environment and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error for the credential-free Claude bridge or if the value
    /// is not a valid bounded environment-variable name.
    pub fn with_api_key_env(
        mut self,
        value: impl Into<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        if matches!(
            self.provider,
            ProjectLlmProvider::ClaudeBridge | ProjectLlmProvider::CliBridge
        ) {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        let value = value.into();
        validate_env_name(&value)?;
        self.credential_intent = ProjectLlmCredentialIntent::Environment(value);
        Ok(self)
    }

    #[must_use]
    /// Returns a copy with all resolved credential material removed.
    pub fn without_credentials(mut self) -> Self {
        self.credential_intent = ProjectLlmCredentialIntent::Clear;
        self
    }

    /// Sets the timeout milliseconds and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if `timeout_ms` is zero or exceeds the project-tier maximum.
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Result<Self, ProjectLlmConfigError> {
        if timeout_ms == 0 || timeout_ms > MAXIMUM_TIMEOUT_MS {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        self.timeout_ms = Some(timeout_ms);
        Ok(self)
    }

    /// Sets the concurrency and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if `concurrency` is zero or exceeds the project-tier maximum.
    pub fn with_concurrency(mut self, concurrency: u16) -> Result<Self, ProjectLlmConfigError> {
        if concurrency == 0 || concurrency > MAXIMUM_CONCURRENCY {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        self.concurrency = Some(concurrency);
        Ok(self)
    }

    /// Sets the ask model and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if the tier is not chat-capable or `ask_model` violates
    /// its non-empty, byte-length, or control-character contract.
    pub fn with_ask_model(
        mut self,
        ask_model: impl Into<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        validate_chat_tier(self.tier)?;
        let ask_model = ask_model.into();
        validate_model(&ask_model)?;
        self.ask_model = Some(ask_model);
        Ok(self)
    }

    /// Sets the summary batch size and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if the tier is not chat-capable or the batch size is
    /// zero or above the summary batching maximum.
    pub fn with_summary_batch_size(
        mut self,
        summary_batch_size: u16,
    ) -> Result<Self, ProjectLlmConfigError> {
        validate_chat_tier(self.tier)?;
        if summary_batch_size == 0 || summary_batch_size > MAXIMUM_SUMMARY_BATCH_SIZE {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        self.summary_batch_size = Some(summary_batch_size);
        Ok(self)
    }

    /// Sets the claude bin and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error unless the provider is Claude bridge, or if the binary
    /// name/path is empty, oversized, or contains control characters.
    pub fn with_claude_bin(
        mut self,
        claude_bin: impl Into<String>,
    ) -> Result<Self, ProjectLlmConfigError> {
        if self.provider != ProjectLlmProvider::ClaudeBridge {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        let claude_bin = claude_bin.into();
        if claude_bin.is_empty()
            || claude_bin.len() > MAXIMUM_CLI_COMMAND_BYTES
            || claude_bin.chars().any(char::is_control)
        {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        self.claude_bin = Some(claude_bin);
        Ok(self)
    }
}

fn validate_cli_bridge_config(config: &CliBridgeConfig) -> Result<(), ProjectLlmConfigError> {
    validate_cli_process_text(&config.command, MAXIMUM_CLI_COMMAND_BYTES)?;
    if config.args.len() > MAXIMUM_CLI_ARGUMENTS {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    let mut argument_bytes = 0_usize;
    let mut prompt_tokens = 0_usize;
    for argument in &config.args {
        validate_cli_process_text(argument, MAXIMUM_CLI_ARGUMENT_BYTES)?;
        argument_bytes = argument_bytes
            .checked_add(argument.len())
            .filter(|total| *total <= MAXIMUM_CLI_ARGUMENT_TOTAL_BYTES)
            .ok_or(ProjectLlmConfigError::InvalidTier)?;
        prompt_tokens = prompt_tokens
            .checked_add(validate_template_tokens(
                argument,
                &["{model}", "{prompt}"],
                "{prompt}",
            )?)
            .ok_or(ProjectLlmConfigError::InvalidTier)?;
    }
    match config.input {
        CliBridgeInputMode::Stdin if prompt_tokens != 0 => {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        CliBridgeInputMode::Arg if prompt_tokens != 1 => {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        CliBridgeInputMode::Stdin | CliBridgeInputMode::Arg => {}
    }
    if config.prompt_template.is_empty()
        || config.prompt_template.len() > MAXIMUM_CLI_PROMPT_TEMPLATE_BYTES
        || config.prompt_template.contains('\0')
        || validate_template_tokens(&config.prompt_template, &["{system}", "{user}"], "{system}")?
            != 1
        || validate_template_tokens(&config.prompt_template, &["{system}", "{user}"], "{user}")?
            != 1
    {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    match (config.response_format, config.response_path.as_deref()) {
        (CliBridgeResponseFormat::JsonPath, Some(path)) => validate_cli_response_path(path),
        (CliBridgeResponseFormat::JsonPath, None)
        | (CliBridgeResponseFormat::Raw | CliBridgeResponseFormat::Claude, Some(_)) => {
            Err(ProjectLlmConfigError::InvalidTier)
        }
        (CliBridgeResponseFormat::Raw | CliBridgeResponseFormat::Claude, None) => Ok(()),
    }
}

fn validate_cli_process_text(value: &str, maximum: usize) -> Result<(), ProjectLlmConfigError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(ProjectLlmConfigError::InvalidTier)
    } else {
        Ok(())
    }
}

fn validate_template_tokens(
    value: &str,
    allowed: &[&str],
    counted: &str,
) -> Result<usize, ProjectLlmConfigError> {
    let mut remaining = value;
    let mut count = 0_usize;
    loop {
        let open = remaining.find('{');
        let close = remaining.find('}');
        match (open, close) {
            (None, None) => return Ok(count),
            (Some(open), Some(close)) if open < close => {
                let token_end = close.saturating_add(1);
                let token = &remaining[open..token_end];
                if !allowed.contains(&token) {
                    return Err(ProjectLlmConfigError::InvalidTier);
                }
                if token == counted {
                    count = count
                        .checked_add(1)
                        .ok_or(ProjectLlmConfigError::InvalidTier)?;
                }
                remaining = &remaining[token_end..];
            }
            _ => return Err(ProjectLlmConfigError::InvalidTier),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CliResponsePathComponent {
    Field(String),
    Index(i64),
}

pub(crate) fn parse_cli_response_path(
    path: &str,
) -> Result<Vec<CliResponsePathComponent>, ProjectLlmConfigError> {
    if path.is_empty()
        || path.len() > MAXIMUM_CLI_RESPONSE_PATH_BYTES
        || !path.is_ascii()
        || path.chars().any(char::is_control)
    {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    let bytes = path.as_bytes();
    let mut index = usize::from(bytes.first() == Some(&b'$'));
    let mut components = Vec::new();
    if index == 0 && !matches!(bytes[0], b'.' | b'[') {
        let (field, next) = parse_cli_response_field(path, index)?;
        components.push(CliResponsePathComponent::Field(field));
        index = next;
    }
    while index < bytes.len() {
        let (component, next) = match bytes[index] {
            b'.' => {
                let (field, next) = parse_cli_response_field(path, index.saturating_add(1))?;
                (CliResponsePathComponent::Field(field), next)
            }
            b'[' => parse_cli_response_index(path, index.saturating_add(1))?,
            _ => return Err(ProjectLlmConfigError::InvalidTier),
        };
        if components.len() >= MAXIMUM_CLI_RESPONSE_PATH_COMPONENTS {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        components.push(component);
        index = next;
    }
    if components.is_empty() {
        Err(ProjectLlmConfigError::InvalidTier)
    } else {
        Ok(components)
    }
}

fn parse_cli_response_field(
    path: &str,
    start: usize,
) -> Result<(String, usize), ProjectLlmConfigError> {
    let bytes = path.as_bytes();
    let mut end = start;
    while end < bytes.len() && !matches!(bytes[end], b'.' | b'[') {
        end = end.saturating_add(1);
    }
    let field = path
        .get(start..end)
        .filter(|field| {
            !field.is_empty()
                && field
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    Ok((field.to_owned(), end))
}

fn parse_cli_response_index(
    path: &str,
    start: usize,
) -> Result<(CliResponsePathComponent, usize), ProjectLlmConfigError> {
    let close = path
        .get(start..)
        .and_then(|remaining| remaining.find(']'))
        .and_then(|offset| start.checked_add(offset))
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    let raw = path
        .get(start..close)
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    let requested = raw
        .parse::<i64>()
        .map_err(|_| ProjectLlmConfigError::InvalidTier)?;
    let next = close
        .checked_add(1)
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    Ok((CliResponsePathComponent::Index(requested), next))
}

fn validate_cli_response_path(path: &str) -> Result<(), ProjectLlmConfigError> {
    parse_cli_response_path(path).map(drop)
}

fn validate_chat_tier(tier: ProjectLlmTier) -> Result<(), ProjectLlmConfigError> {
    if matches!(
        tier,
        ProjectLlmTier::Embedding | ProjectLlmTier::Reranker | ProjectLlmTier::Decision
    ) {
        Err(ProjectLlmConfigError::InvalidTier)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
/// Errors produced while processing project LLM config.
pub enum ProjectLlmConfigError {
    #[error("Cartograph project configuration path is unavailable")]
    /// The requested project could not be opened safely.
    ProjectUnavailable,
    #[error("Cartograph project configuration is too large")]
    /// The host configuration exceeds the safe rewrite byte ceiling.
    ConfigTooLarge,
    #[error("Cartograph project configuration is invalid")]
    /// Configuration is malformed or violates a required bound.
    InvalidConfig,
    #[error(
        "Cartograph project configuration field `{field}` must be between {minimum} and {maximum}"
    )]
    /// A named numeric configuration field is outside its documented range.
    NumericFieldOutOfRange {
        /// Stable public configuration field name.
        field: &'static str,
        /// Inclusive minimum accepted value.
        minimum: u64,
        /// Inclusive maximum accepted value.
        maximum: u64,
    },
    #[error("Cartograph project LLM tier is invalid")]
    /// The requested model tier is not valid for this provider.
    InvalidTier,
    #[error("Cartograph project LLM credential environment variable is unavailable")]
    /// The configured credential reference could not be resolved privately.
    CredentialUnavailable,
    #[error("Cartograph project configuration cannot be written safely")]
    /// The bounded output could not be written atomically.
    WriteFailed,
    #[error("Cartograph project configuration changed concurrently")]
    /// The host configuration changed during the atomic rewrite.
    ConcurrentModification,
}

/// Load one tier, retaining v1's ask/classify-to-summarize fallback.
/// # Errors
///
/// Returns an error if the project/config path is unsafe/unreadable/oversized,
/// JSON or the selected/fallback tier is malformed, or credential lookup fails.
pub fn load_project_llm_tier(
    project_root: &Path,
    tier: ProjectLlmTier,
) -> Result<Option<ProjectLlmTierConfig>, ProjectLlmConfigError> {
    load_project_llm_tier_with_fallback(project_root, tier, true)
}

/// Load only the named tier without ask/local/classify fallback. Diagnostics
/// use this to distinguish a deliberate split tier from summarize fallback.
/// # Errors
///
/// Returns an error if the project/config path is unsafe/unreadable/oversized,
/// JSON or the exact tier is malformed, or credential lookup fails.
pub fn load_exact_project_llm_tier(
    project_root: &Path,
    tier: ProjectLlmTier,
) -> Result<Option<ProjectLlmTierConfig>, ProjectLlmConfigError> {
    load_project_llm_tier_with_fallback(project_root, tier, false)
}

/// Read the v1-compatible project-wide source-file ceiling without conflating
/// an absent setting with an invalid configuration file.
/// # Errors
///
/// Returns an error if project source configuration is unsafe, unreadable,
/// oversized, malformed, or contains an out-of-range file-size value.
pub fn load_project_max_file_size(
    project_root: &Path,
) -> Result<Option<usize>, ProjectLlmConfigError> {
    load_project_source_settings(project_root).map(|settings| settings.maximum_file_bytes())
}

/// Read the summary candidate floor and eager budget from the shared v1 config.
/// # Errors
///
/// Returns an error if config access/JSON shape is invalid or summary enable,
/// eager-limit, line-floor, kind-override, or depth settings violate bounds.
pub fn load_project_summary_settings(
    project_root: &Path,
) -> Result<ProjectSummarySettings, ProjectLlmConfigError> {
    let Some(value) = read_config_value(project_root)? else {
        return Ok(default_summary_settings(false));
    };
    let root = value
        .as_object()
        .ok_or(ProjectLlmConfigError::InvalidConfig)?;
    let Some(llm) = root.get("llm") else {
        return Ok(default_summary_settings(false));
    };
    let llm = llm
        .as_object()
        .ok_or(ProjectLlmConfigError::InvalidConfig)?;
    let provider_enabled = optional_config_bool(llm, "enabled")?.unwrap_or(true);
    let enabled = provider_enabled && optional_config_bool(llm, "summarize")?.unwrap_or(true);
    let eager_limit = llm
        .get("summarizeEagerLimit")
        .map(parse_summary_eager_limit)
        .transpose()?
        .unwrap_or(ProjectSummaryEagerLimit::Bounded(
            DEFAULT_SUMMARY_EAGER_LIMIT,
        ));
    let minimum_body_lines = llm
        .get("minBodyLines")
        .map(parse_summary_line_floor)
        .transpose()?
        .unwrap_or(DEFAULT_SUMMARY_MINIMUM_BODY_LINES);
    let mut minimum_body_lines_by_kind = BTreeMap::from([("route".to_owned(), 1)]);
    if let Some(overrides) = llm.get("minBodyLinesByKind") {
        let overrides = overrides
            .as_object()
            .filter(|values| values.len() <= MAXIMUM_SUMMARY_KIND_OVERRIDES)
            .ok_or(ProjectLlmConfigError::InvalidConfig)?;
        for (kind, value) in overrides {
            if kind.is_empty()
                || kind.len() > MAXIMUM_SUMMARY_KIND_BYTES
                || kind.chars().any(char::is_control)
            {
                return Err(ProjectLlmConfigError::InvalidConfig);
            }
            minimum_body_lines_by_kind.insert(kind.clone(), parse_summary_line_floor(value)?);
        }
    }
    Ok(ProjectSummarySettings {
        enabled,
        eager_limit,
        minimum_body_lines,
        minimum_body_lines_by_kind,
    })
}

fn default_summary_settings(enabled: bool) -> ProjectSummarySettings {
    ProjectSummarySettings {
        enabled,
        eager_limit: ProjectSummaryEagerLimit::Bounded(DEFAULT_SUMMARY_EAGER_LIMIT),
        minimum_body_lines: DEFAULT_SUMMARY_MINIMUM_BODY_LINES,
        minimum_body_lines_by_kind: BTreeMap::from([("route".to_owned(), 1)]),
    }
}

fn parse_summary_eager_limit(
    value: &Value,
) -> Result<ProjectSummaryEagerLimit, ProjectLlmConfigError> {
    let value = value
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or(ProjectLlmConfigError::InvalidConfig)?;
    if value < 0.0 {
        return Ok(ProjectSummaryEagerLimit::Uncapped);
    }
    let maximum = MAXIMUM_SUMMARY_EAGER_LIMIT
        .to_f64()
        .ok_or(ProjectLlmConfigError::InvalidConfig)?;
    if value > maximum {
        return Err(ProjectLlmConfigError::InvalidConfig);
    }
    let bounded = value
        .ceil()
        .to_u64()
        .filter(|bounded| *bounded <= MAXIMUM_SUMMARY_EAGER_LIMIT)
        .ok_or(ProjectLlmConfigError::InvalidConfig)?;
    Ok(ProjectSummaryEagerLimit::Bounded(bounded))
}

fn parse_summary_line_floor(value: &Value) -> Result<u32, ProjectLlmConfigError> {
    let value = value
        .as_f64()
        .filter(|value| {
            value.is_finite()
                && *value >= 0.0
                && *value <= f64::from(MAXIMUM_SUMMARY_MINIMUM_BODY_LINES)
        })
        .ok_or(ProjectLlmConfigError::InvalidConfig)?;
    value
        .ceil()
        .to_u32()
        .filter(|bounded| *bounded <= MAXIMUM_SUMMARY_MINIMUM_BODY_LINES)
        .ok_or(ProjectLlmConfigError::InvalidConfig)
}

fn optional_config_bool(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, ProjectLlmConfigError> {
    object
        .get(key)
        .map(|value| value.as_bool().ok_or(ProjectLlmConfigError::InvalidConfig))
        .transpose()
}

fn load_project_llm_tier_with_fallback(
    project_root: &Path,
    tier: ProjectLlmTier,
    allow_fallback: bool,
) -> Result<Option<ProjectLlmTierConfig>, ProjectLlmConfigError> {
    let Some(value) = read_config_value(project_root)? else {
        return Ok(None);
    };
    let llm = value
        .as_object()
        .and_then(|root| root.get("llm"))
        .and_then(Value::as_object);
    let Some(llm) = llm else {
        return Ok(None);
    };
    if llm.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Ok(None);
    }
    let exact = llm.get(tier.config_key()).filter(|value| !value.is_null());
    let selected = exact.map(|value| (value, tier, false)).or_else(|| {
        if !allow_fallback {
            return None;
        }
        tier.fallback().and_then(|fallback| {
            llm.get(fallback.config_key())
                .filter(|value| !value.is_null())
                .map(|value| (value, fallback, true))
        })
    });
    selected
        .map(|(value, configured_tier, fallback)| {
            parse_tier(
                value,
                TierResolution {
                    configured: configured_tier,
                    requested: tier,
                    fallback,
                },
            )
        })
        .transpose()
}

/// Atomically update one or more tier blocks while preserving non-LLM config.
/// # Errors
///
/// Returns an error if tier mutations are invalid/empty, existing config is
/// unsafe/malformed/oversized, or the locked private atomic rewrite fails.
pub fn write_project_llm_tiers(
    project_root: &Path,
    inputs: &[ProjectLlmTierInput],
) -> Result<(), ProjectLlmConfigError> {
    write_project_llm_configuration(project_root, inputs, &[])
}

/// Replace legacy inline credentials with named environment references only
/// when the current process proves that the selected environment variable
/// contains the exact same secret. The update is one atomic config write and
/// reports no secret material.
/// # Errors
///
/// Returns an error if overrides/config are invalid, an environment value is
/// absent or differs from the inline secret, or guarded atomic rewrite detects a race.
pub fn migrate_project_inline_credentials(
    project_root: &Path,
    environment_overrides: &[(ProjectLlmTier, String)],
    apply: bool,
) -> Result<ProjectCredentialMigrationReport, ProjectLlmConfigError> {
    migrate_project_inline_credentials_with(CredentialMigrationRequest {
        project_root,
        environment_overrides,
        apply,
        resolve: |name: &str| env::var(name).ok(),
    })
}

struct CredentialMigrationRequest<'a, Resolve> {
    project_root: &'a Path,
    environment_overrides: &'a [(ProjectLlmTier, String)],
    apply: bool,
    resolve: Resolve,
}

struct CredentialMigrationState<Resolve> {
    overrides: BTreeMap<ProjectLlmTier, String>,
    apply: bool,
    resolve: Resolve,
    candidates: Vec<ProjectCredentialMigrationEntry>,
    migrated: usize,
}

impl<Resolve> CredentialMigrationState<Resolve>
where
    Resolve: FnMut(&str) -> Option<String>,
{
    fn new(
        environment_overrides: &[(ProjectLlmTier, String)],
        apply: bool,
        resolve: Resolve,
    ) -> Result<Self, ProjectLlmConfigError> {
        let mut overrides = BTreeMap::new();
        for (tier, name) in environment_overrides {
            validate_env_name(name)?;
            if overrides.insert(*tier, name.clone()).is_some() {
                return Err(ProjectLlmConfigError::InvalidTier);
            }
        }
        Ok(Self {
            overrides,
            apply,
            resolve,
            candidates: Vec::new(),
            migrated: 0,
        })
    }

    fn migrate_tier(
        &mut self,
        llm: &mut Map<String, Value>,
        tier: ProjectLlmTier,
    ) -> Result<(), ProjectLlmConfigError> {
        let Some(value) = llm.get_mut(tier.config_key()) else {
            return Ok(());
        };
        if value.is_null() {
            return Ok(());
        }
        let object = value
            .as_object_mut()
            .ok_or(ProjectLlmConfigError::InvalidTier)?;
        let Some(inline) = optional_string_value(object, "apiKey")? else {
            return Ok(());
        };
        validate_api_key(inline)?;
        if object.contains_key("apiKeyEnv") {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        let provider = object
            .get("provider")
            .and_then(Value::as_str)
            .and_then(ProjectLlmProvider::parse)
            .ok_or(ProjectLlmConfigError::InvalidTier)?;
        let environment = self
            .overrides
            .get(&tier)
            .cloned()
            .or_else(|| match provider {
                ProjectLlmProvider::OpenAiCompat => Some("OPENAI_API_KEY".to_owned()),
                ProjectLlmProvider::AnthropicApi => Some("ANTHROPIC_API_KEY".to_owned()),
                ProjectLlmProvider::Typesafe => Some("TYPESAFE_API_KEY".to_owned()),
                ProjectLlmProvider::ClaudeBridge | ProjectLlmProvider::CliBridge => None,
            });
        let status = match environment.as_deref() {
            None => ProjectCredentialMigrationStatus::UnsupportedProvider,
            Some(name) => match (self.resolve)(name) {
                None => ProjectCredentialMigrationStatus::EnvironmentMissing,
                Some(value) if value != inline => {
                    ProjectCredentialMigrationStatus::EnvironmentMismatch
                }
                Some(_) if self.apply => {
                    object.remove("apiKey");
                    object.insert("apiKeyEnv".to_owned(), Value::String(name.to_owned()));
                    self.migrated = self.migrated.saturating_add(1);
                    ProjectCredentialMigrationStatus::Migrated
                }
                Some(_) => ProjectCredentialMigrationStatus::Ready,
            },
        };
        self.candidates.push(ProjectCredentialMigrationEntry {
            tier,
            environment,
            status,
        });
        Ok(())
    }

    fn report(self) -> ProjectCredentialMigrationReport {
        let remaining_inline = self.candidates.len().saturating_sub(self.migrated);
        ProjectCredentialMigrationReport {
            dry_run: !self.apply,
            candidates: self.candidates,
            migrated: self.migrated,
            remaining_inline,
        }
    }
}

fn migrate_project_inline_credentials_with<Resolve>(
    request: CredentialMigrationRequest<'_, Resolve>,
) -> Result<ProjectCredentialMigrationReport, ProjectLlmConfigError>
where
    Resolve: FnMut(&str) -> Option<String>,
{
    migrate_project_inline_credentials_with_observer(request, || {})
}

fn migrate_project_inline_credentials_with_observer<Resolve, Observe>(
    request: CredentialMigrationRequest<'_, Resolve>,
    observe_before_write: Observe,
) -> Result<ProjectCredentialMigrationReport, ProjectLlmConfigError>
where
    Resolve: FnMut(&str) -> Option<String>,
    Observe: FnOnce(),
{
    let mut state = CredentialMigrationState::new(
        request.environment_overrides,
        request.apply,
        request.resolve,
    )?;
    let Some(snapshot) = read_config_snapshot(request.project_root)? else {
        return Ok(state.report());
    };
    let mut config = snapshot.value;
    let root = config
        .as_object_mut()
        .ok_or(ProjectLlmConfigError::InvalidConfig)?;
    let Some(llm) = root.get_mut("llm").and_then(Value::as_object_mut) else {
        return Ok(state.report());
    };
    for tier in credential_migration_tiers() {
        state.migrate_tier(llm, tier)?;
    }
    if state.apply && state.migrated > 0 {
        observe_before_write();
        write_config_value_if_unchanged(request.project_root, &config, &snapshot.bytes)?;
    }
    Ok(state.report())
}

const fn credential_migration_tiers() -> [ProjectLlmTier; 7] {
    [
        ProjectLlmTier::Embedding,
        ProjectLlmTier::Summarize,
        ProjectLlmTier::Local,
        ProjectLlmTier::Ask,
        ProjectLlmTier::Classify,
        ProjectLlmTier::Reranker,
        ProjectLlmTier::Decision,
    ]
}

/// Atomically update configured tiers and explicitly disable incompatible ones.
/// # Errors
///
/// Returns an error if no mutation is requested, tier sets/config objects are
/// invalid, or the private locked size-bounded atomic rewrite fails.
pub fn write_project_llm_configuration(
    project_root: &Path,
    inputs: &[ProjectLlmTierInput],
    cleared: &[ProjectLlmTier],
) -> Result<(), ProjectLlmConfigError> {
    write_project_llm_configuration_with_report(project_root, inputs, cleared).map(|_| ())
}

/// Atomically update configured tiers and return secret-free credential actions.
/// # Errors
///
/// Returns an error if no mutation is requested, tier sets/config objects are
/// invalid, or the private locked size-bounded atomic rewrite fails.
pub fn write_project_llm_configuration_with_report(
    project_root: &Path,
    inputs: &[ProjectLlmTierInput],
    cleared: &[ProjectLlmTier],
) -> Result<ProjectLlmWriteReport, ProjectLlmConfigError> {
    if inputs.is_empty() && cleared.is_empty() {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    update_config_value(project_root, |current| {
        let mut value = current.unwrap_or_else(|| json!({"version": 2}));
        let root = value
            .as_object_mut()
            .ok_or(ProjectLlmConfigError::InvalidConfig)?;
        let llm = object_field(root, "llm")?;
        llm.insert("enabled".to_owned(), Value::Bool(true));
        let mut credential_actions = Vec::with_capacity(inputs.len());
        for input in inputs {
            credential_actions.push(update_project_llm_tier(llm, input)?);
        }
        for tier in cleared {
            llm.insert(tier.config_key().to_owned(), Value::Null);
        }
        Ok((value, ProjectLlmWriteReport { credential_actions }))
    })
}

fn update_project_llm_tier(
    llm: &mut Map<String, Value>,
    input: &ProjectLlmTierInput,
) -> Result<ProjectLlmCredentialWriteEntry, ProjectLlmConfigError> {
    let tier = object_field(llm, input.tier.config_key())?;
    let action = apply_credential_intent(tier, input);
    write_project_llm_provider(tier, input);
    write_project_llm_optional_settings(tier, input);
    write_project_llm_cli_bridge(tier, input.cli_bridge.as_ref());
    Ok(ProjectLlmCredentialWriteEntry {
        tier: input.tier,
        action,
    })
}

fn write_project_llm_provider(tier: &mut Map<String, Value>, input: &ProjectLlmTierInput) {
    tier.insert(
        "provider".to_owned(),
        Value::String(input.provider.as_str().to_owned()),
    );
    if matches!(
        input.provider,
        ProjectLlmProvider::ClaudeBridge | ProjectLlmProvider::CliBridge
    ) {
        tier.remove("endpoint");
    } else {
        tier.insert("endpoint".to_owned(), Value::String(input.endpoint.clone()));
    }
    tier.insert("model".to_owned(), Value::String(input.model.clone()));
    if let Some(claude_bin) = &input.claude_bin {
        tier.insert("claudeBin".to_owned(), Value::String(claude_bin.clone()));
    } else if input.provider != ProjectLlmProvider::ClaudeBridge {
        tier.remove("claudeBin");
    }
}

fn write_project_llm_optional_settings(tier: &mut Map<String, Value>, input: &ProjectLlmTierInput) {
    if let Some(features) = &input.decision_features {
        tier.insert(
            "features".to_owned(),
            Value::Array(features.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(ask_model) = &input.ask_model {
        tier.insert("askModel".to_owned(), Value::String(ask_model.clone()));
    }
    if let Some(timeout_ms) = input.timeout_ms {
        tier.insert("timeoutMs".to_owned(), Value::from(timeout_ms));
    }
    if let Some(concurrency) = input.concurrency {
        tier.insert("concurrency".to_owned(), Value::from(concurrency));
    }
    if let Some(summary_batch_size) = input.summary_batch_size {
        tier.insert(
            "summaryBatchSize".to_owned(),
            Value::from(summary_batch_size),
        );
    }
}

fn write_project_llm_cli_bridge(
    tier: &mut Map<String, Value>,
    cli_bridge: Option<&CliBridgeConfig>,
) {
    if let Some(cli_bridge) = cli_bridge {
        tier.insert(
            "command".to_owned(),
            Value::String(cli_bridge.command.clone()),
        );
        tier.insert(
            "args".to_owned(),
            Value::Array(cli_bridge.args.iter().cloned().map(Value::String).collect()),
        );
        tier.insert(
            "input".to_owned(),
            Value::String(cli_bridge.input.as_str().to_owned()),
        );
        if cli_bridge.prompt_template == DEFAULT_CLI_PROMPT_TEMPLATE {
            tier.remove("promptTemplate");
        } else {
            tier.insert(
                "promptTemplate".to_owned(),
                Value::String(cli_bridge.prompt_template.clone()),
            );
        }
        tier.insert(
            "responseFormat".to_owned(),
            Value::String(cli_bridge.response_format.as_str().to_owned()),
        );
        if let Some(response_path) = &cli_bridge.response_path {
            tier.insert(
                "responsePath".to_owned(),
                Value::String(response_path.clone()),
            );
        } else {
            tier.remove("responsePath");
        }
    } else {
        for key in [
            "command",
            "args",
            "input",
            "promptTemplate",
            "responseFormat",
            "responsePath",
        ] {
            tier.remove(key);
        }
    }
}

fn apply_credential_intent(
    tier: &mut Map<String, Value>,
    input: &ProjectLlmTierInput,
) -> ProjectLlmCredentialWriteAction {
    let had_credentials = tier.contains_key("apiKey") || tier.contains_key("apiKeyEnv");
    match &input.credential_intent {
        ProjectLlmCredentialIntent::Clear => {
            tier.remove("apiKey");
            tier.remove("apiKeyEnv");
            ProjectLlmCredentialWriteAction::ClearedExplicitly
        }
        ProjectLlmCredentialIntent::Environment(environment) => {
            tier.remove("apiKey");
            tier.insert("apiKeyEnv".to_owned(), Value::String(environment.clone()));
            ProjectLlmCredentialWriteAction::EnvironmentReferenceSet
        }
        ProjectLlmCredentialIntent::Preserve
            if had_credentials && credential_origin_changed(tier, input) =>
        {
            tier.remove("apiKey");
            tier.remove("apiKeyEnv");
            ProjectLlmCredentialWriteAction::ClearedOriginChange
        }
        ProjectLlmCredentialIntent::Preserve if had_credentials => {
            ProjectLlmCredentialWriteAction::Preserved
        }
        ProjectLlmCredentialIntent::Preserve => ProjectLlmCredentialWriteAction::Unchanged,
    }
}

fn credential_origin_changed(configured: &Map<String, Value>, input: &ProjectLlmTierInput) -> bool {
    configured_credential_origin(configured)
        .zip(credential_origin(input.provider, &input.endpoint))
        .is_none_or(|(configured, requested)| configured != requested)
}

fn configured_credential_origin(
    configured: &Map<String, Value>,
) -> Option<(ProjectLlmProvider, String)> {
    let provider = configured
        .get("provider")
        .and_then(Value::as_str)
        .and_then(ProjectLlmProvider::parse)?;
    let endpoint = match provider {
        ProjectLlmProvider::OpenAiCompat => configured
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or(OPENAI_CLOUD_ENDPOINT),
        ProjectLlmProvider::AnthropicApi => configured
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or(ANTHROPIC_CLOUD_ENDPOINT),
        ProjectLlmProvider::ClaudeBridge => CLAUDE_BRIDGE_ENDPOINT,
        ProjectLlmProvider::Typesafe => configured
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or(crate::jev::JEV_ENDPOINT),
        ProjectLlmProvider::CliBridge => CLI_BRIDGE_ENDPOINT,
    };
    credential_origin(provider, endpoint)
}

fn credential_origin(
    provider: ProjectLlmProvider,
    endpoint: &str,
) -> Option<(ProjectLlmProvider, String)> {
    if matches!(
        provider,
        ProjectLlmProvider::ClaudeBridge | ProjectLlmProvider::CliBridge
    ) {
        return Some((provider, endpoint.to_owned()));
    }
    let endpoint = Url::parse(endpoint).ok()?;
    let host = endpoint
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let port = endpoint.port_or_known_default()?;
    Some((
        provider,
        format!("{}://{host}:{port}", endpoint.scheme().to_ascii_lowercase()),
    ))
}

/// Change only one configured tier's bounded client concurrency.
/// # Errors
///
/// Returns an error if `concurrency` is out of range, the exact configured tier
/// is missing/malformed, or the private atomic rewrite fails.
pub fn tune_project_llm_tier(
    project_root: &Path,
    tier: ProjectLlmTier,
    concurrency: u16,
) -> Result<(), ProjectLlmConfigError> {
    if concurrency == 0 || concurrency > MAXIMUM_CONCURRENCY {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    update_config_value(project_root, |current| {
        let mut value = current.ok_or(ProjectLlmConfigError::InvalidConfig)?;
        let root = value
            .as_object_mut()
            .ok_or(ProjectLlmConfigError::InvalidConfig)?;
        let llm = root
            .get_mut("llm")
            .and_then(Value::as_object_mut)
            .ok_or(ProjectLlmConfigError::InvalidConfig)?;
        let tier = llm
            .get_mut(tier.config_key())
            .and_then(Value::as_object_mut)
            .ok_or(ProjectLlmConfigError::InvalidTier)?;
        tier.insert("concurrency".to_owned(), Value::from(concurrency));
        Ok((value, ()))
    })
}

/// Probe a validated endpoint without credentials, redirects, or unbounded reads.
/// # Errors
///
/// Returns an error if endpoint/timeout validation fails, no TLS provider can
/// be installed, or the redirect-free bounded HTTP client cannot be built.
pub async fn probe_openai_compatible_endpoint(
    endpoint: &str,
    timeout: std::time::Duration,
) -> Result<LlmEndpointProbe, ProjectLlmConfigError> {
    validate_endpoint(endpoint)?;
    if timeout.is_zero() || timeout > std::time::Duration::from_secs(10) {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    crate::ensure_tls_crypto_provider().map_err(|_| ProjectLlmConfigError::InvalidTier)?;
    let models_url =
        crate::endpoint::normalize_endpoint(endpoint, crate::endpoint::EndpointPath::Models)
            .map_err(|()| ProjectLlmConfigError::InvalidTier)?;
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ProjectLlmConfigError::InvalidTier)?;
    let Ok(response) = client.get(models_url).send().await else {
        return Ok(unreachable_probe());
    };
    if response.status() != StatusCode::OK {
        return Ok(reachable_incompatible_probe());
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAXIMUM_PROBE_BYTES as u64)
    {
        return Ok(reachable_incompatible_probe());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return Ok(reachable_incompatible_probe());
        };
        if bytes.len().saturating_add(chunk.len()) > MAXIMUM_PROBE_BYTES {
            return Ok(reachable_incompatible_probe());
        }
        bytes.extend_from_slice(&chunk);
    }
    let Some(data) = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value.get("data").and_then(Value::as_array).cloned())
    else {
        return Ok(reachable_incompatible_probe());
    };
    let mut models = data
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .filter(|model| {
            !model.is_empty()
                && model.len() <= MAXIMUM_MODEL_BYTES
                && !model.chars().any(char::is_control)
        })
        .take(MAXIMUM_PROBE_MODELS)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    Ok(LlmEndpointProbe {
        reachable: true,
        openai_compatible: true,
        models,
    })
}

const fn unreachable_probe() -> LlmEndpointProbe {
    LlmEndpointProbe {
        reachable: false,
        openai_compatible: false,
        models: Vec::new(),
    }
}

const fn reachable_incompatible_probe() -> LlmEndpointProbe {
    LlmEndpointProbe {
        reachable: true,
        openai_compatible: false,
        models: Vec::new(),
    }
}

#[derive(Clone, Copy)]
struct TierResolution {
    configured: ProjectLlmTier,
    requested: ProjectLlmTier,
    fallback: bool,
}

fn parse_tier(
    value: &Value,
    resolution: TierResolution,
) -> Result<ProjectLlmTierConfig, ProjectLlmConfigError> {
    let value = value
        .as_object()
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    let provider = value
        .get("provider")
        .and_then(Value::as_str)
        .and_then(ProjectLlmProvider::parse)
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    let chat_tier = !matches!(
        resolution.configured,
        ProjectLlmTier::Embedding | ProjectLlmTier::Reranker
    );
    if (resolution.configured == ProjectLlmTier::Decision)
        != (provider == ProjectLlmProvider::Typesafe)
        || (!chat_tier && provider != ProjectLlmProvider::OpenAiCompat)
    {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    let endpoint = project_tier_endpoint(value, provider)?;
    let model = project_tier_model(value, provider, resolution)?;
    let ask_model = optional_model(value, "askModel")?;
    validate_model(&model)?;
    let credentials = parse_tier_credentials(value, provider)?;
    let limits = parse_tier_runtime_limits(value)?;
    let claude_bin = parse_claude_bin(value, provider)?;
    let cli_bridge = parse_cli_bridge(value, provider)?;
    let llama_server_args = parse_llama_server_args(value)?;
    let externally_managed = optional_bool(value, "externallyManaged")?.unwrap_or(false);
    let decision_features = if resolution.configured == ProjectLlmTier::Decision {
        parse_decision_features(value)?
    } else {
        None
    };
    Ok(ProjectLlmTierConfig {
        provider,
        endpoint,
        model,
        ask_model,
        api_key: credentials.api_key,
        unavailable_credential_env: credentials.unavailable_environment,
        credential_source: credentials.source,
        timeout_ms: limits.timeout_ms,
        concurrency: limits.concurrency,
        summary_batch_size: limits.summary_batch_size,
        claude_bin,
        cli_bridge,
        llama_server_args,
        externally_managed,
        decision_features,
    })
}

/// Bounded feature allowlist for the decision tier. Unknown names are kept
/// so a newer configuration stays readable; consumers ignore them.
fn parse_decision_features(
    object: &Map<String, Value>,
) -> Result<Option<Vec<String>>, ProjectLlmConfigError> {
    const MAXIMUM_FEATURES: usize = 16;
    const MAXIMUM_FEATURE_BYTES: usize = 64;
    let Some(value) = object.get("features") else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .filter(|values| values.len() <= MAXIMUM_FEATURES)
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= MAXIMUM_FEATURE_BYTES
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                })
                .map(str::to_owned)
                .ok_or(ProjectLlmConfigError::InvalidTier)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

struct TierCredentials {
    api_key: Option<SecretString>,
    source: ProjectLlmCredentialSource,
    unavailable_environment: Option<String>,
}

#[derive(Clone, Copy)]
struct CredentialResolution<'input> {
    value: &'input Map<String, Value>,
    explicit_api_key_env: Option<&'input str>,
    inline: Option<&'input str>,
    api_key_env: Option<&'input str>,
    missing_default_env_permitted: bool,
    missing_decision_env_permitted: bool,
}

fn parse_tier_credentials(
    value: &Map<String, Value>,
    provider: ProjectLlmProvider,
) -> Result<TierCredentials, ProjectLlmConfigError> {
    let explicit_api_key_env = optional_string_value(value, "apiKeyEnv")?;
    let inline = optional_string_value(value, "apiKey")?;
    let api_key_env = explicit_api_key_env
        .or_else(|| (provider == ProjectLlmProvider::Typesafe).then_some("TYPESAFE_API_KEY"))
        .or_else(|| (provider == ProjectLlmProvider::AnthropicApi).then_some("ANTHROPIC_API_KEY"))
        .or_else(|| {
            (provider == ProjectLlmProvider::OpenAiCompat && value.get("endpoint").is_none())
                .then_some("OPENAI_API_KEY")
        });
    if explicit_api_key_env.is_some() && inline.is_some() {
        return Err(ProjectLlmConfigError::InvalidTier);
    }
    if matches!(
        provider,
        ProjectLlmProvider::ClaudeBridge | ProjectLlmProvider::CliBridge
    ) {
        if inline.is_some() || explicit_api_key_env.is_some() {
            return Err(ProjectLlmConfigError::InvalidTier);
        }
        return Ok(TierCredentials {
            api_key: None,
            source: ProjectLlmCredentialSource::None,
            unavailable_environment: None,
        });
    }
    resolve_tier_credentials(CredentialResolution {
        value,
        explicit_api_key_env,
        inline,
        api_key_env,
        missing_default_env_permitted: provider == ProjectLlmProvider::OpenAiCompat,
        missing_decision_env_permitted: provider == ProjectLlmProvider::Typesafe,
    })
}

fn optional_string_value<'input>(
    value: &'input Map<String, Value>,
    key: &str,
) -> Result<Option<&'input str>, ProjectLlmConfigError> {
    match value.get(key) {
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(ProjectLlmConfigError::InvalidTier),
        None => Ok(None),
    }
}

fn resolve_tier_credentials(
    resolution: CredentialResolution<'_>,
) -> Result<TierCredentials, ProjectLlmConfigError> {
    let (api_key, source) = if let Some(key) = resolution.inline {
        validate_api_key(key)?;
        (
            Some(SecretString::from(key.to_owned())),
            ProjectLlmCredentialSource::InlineLegacy,
        )
    } else if let Some(name) = resolution.api_key_env {
        validate_env_name(name)?;
        match env::var(name) {
            Ok(key) => {
                validate_api_key(&key)?;
                (
                    Some(SecretString::from(key)),
                    ProjectLlmCredentialSource::Environment,
                )
            }
            Err(env::VarError::NotPresent) if resolution.missing_decision_env_permitted => {
                return Ok(TierCredentials {
                    api_key: None,
                    source: ProjectLlmCredentialSource::Environment,
                    unavailable_environment: Some(name.to_owned()),
                });
            }
            Err(env::VarError::NotPresent)
                if resolution.explicit_api_key_env.is_none()
                    && resolution.value.get("endpoint").is_some()
                    && resolution.missing_default_env_permitted =>
            {
                (None, ProjectLlmCredentialSource::None)
            }
            Err(_) => return Err(ProjectLlmConfigError::CredentialUnavailable),
        }
    } else {
        (None, ProjectLlmCredentialSource::None)
    };
    Ok(TierCredentials {
        api_key,
        source,
        unavailable_environment: None,
    })
}

struct TierRuntimeLimits {
    timeout_ms: Option<u64>,
    concurrency: Option<u16>,
    summary_batch_size: Option<u16>,
}

fn parse_tier_runtime_limits(
    value: &Map<String, Value>,
) -> Result<TierRuntimeLimits, ProjectLlmConfigError> {
    let timeout_ms = optional_bounded_u64(
        value,
        "timeoutMs",
        BoundedU64Field {
            maximum: MAXIMUM_TIMEOUT_MS,
            invalid: ProjectLlmConfigError::InvalidTier,
        },
    )?;
    let concurrency = optional_bounded_u64(
        value,
        "concurrency",
        BoundedU64Field {
            maximum: u64::from(MAXIMUM_CONCURRENCY),
            invalid: ProjectLlmConfigError::InvalidTier,
        },
    )?
    .map(|value| u16::try_from(value).map_err(|_| ProjectLlmConfigError::InvalidTier))
    .transpose()?;
    let summary_batch_size = optional_bounded_u64(
        value,
        "summaryBatchSize",
        BoundedU64Field {
            maximum: u64::from(MAXIMUM_SUMMARY_BATCH_SIZE),
            invalid: ProjectLlmConfigError::InvalidTier,
        },
    )?
    .map(|value| u16::try_from(value).map_err(|_| ProjectLlmConfigError::InvalidTier))
    .transpose()?;
    Ok(TierRuntimeLimits {
        timeout_ms,
        concurrency,
        summary_batch_size,
    })
}

fn project_tier_endpoint(
    object: &Map<String, Value>,
    provider: ProjectLlmProvider,
) -> Result<String, ProjectLlmConfigError> {
    let configured = match object.get("endpoint") {
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err(ProjectLlmConfigError::InvalidTier),
        None => None,
    };
    let endpoint = match provider {
        ProjectLlmProvider::OpenAiCompat => configured.unwrap_or(OPENAI_CLOUD_ENDPOINT),
        ProjectLlmProvider::AnthropicApi => configured.unwrap_or(ANTHROPIC_CLOUD_ENDPOINT),
        ProjectLlmProvider::Typesafe => configured.unwrap_or(crate::jev::JEV_ENDPOINT),
        ProjectLlmProvider::ClaudeBridge => {
            if configured.is_some() {
                return Err(ProjectLlmConfigError::InvalidTier);
            }
            return Ok(CLAUDE_BRIDGE_ENDPOINT.to_owned());
        }
        ProjectLlmProvider::CliBridge => {
            if configured.is_some() {
                return Err(ProjectLlmConfigError::InvalidTier);
            }
            return Ok(CLI_BRIDGE_ENDPOINT.to_owned());
        }
    };
    validate_endpoint(endpoint)?;
    Ok(endpoint.to_owned())
}

fn project_tier_model(
    object: &Map<String, Value>,
    provider: ProjectLlmProvider,
    resolution: TierResolution,
) -> Result<String, ProjectLlmConfigError> {
    if resolution.fallback && resolution.requested == ProjectLlmTier::Ask {
        if let Some(model) = optional_model(object, "askModel")? {
            return Ok(model);
        }
        if matches!(
            provider,
            ProjectLlmProvider::ClaudeBridge | ProjectLlmProvider::AnthropicApi
        ) {
            return Ok(DEFAULT_CLAUDE_ASK_MODEL.to_owned());
        }
    }
    if let Some(model) = optional_model(object, "model")? {
        return Ok(model);
    }
    match provider {
        ProjectLlmProvider::Typesafe => Ok(crate::jev::JEV_MODEL.to_owned()),
        ProjectLlmProvider::OpenAiCompat | ProjectLlmProvider::CliBridge => {
            Err(ProjectLlmConfigError::InvalidTier)
        }
        ProjectLlmProvider::ClaudeBridge | ProjectLlmProvider::AnthropicApi => {
            let model = if resolution.configured == ProjectLlmTier::Summarize {
                DEFAULT_CLAUDE_SUMMARIZE_MODEL
            } else {
                DEFAULT_CLAUDE_ASK_MODEL
            };
            Ok(model.to_owned())
        }
    }
}

fn optional_model(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ProjectLlmConfigError> {
    object
        .get(key)
        .map(|value| {
            let model = value.as_str().ok_or(ProjectLlmConfigError::InvalidTier)?;
            validate_model(model)?;
            Ok(model.to_owned())
        })
        .transpose()
}

fn parse_claude_bin(
    object: &Map<String, Value>,
    provider: ProjectLlmProvider,
) -> Result<Option<String>, ProjectLlmConfigError> {
    let configured = object
        .get("claudeBin")
        .map(|value| {
            value
                .as_str()
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= MAXIMUM_CLI_COMMAND_BYTES
                        && !value.chars().any(char::is_control)
                })
                .map(str::to_owned)
                .ok_or(ProjectLlmConfigError::InvalidTier)
        })
        .transpose()?;
    if provider == ProjectLlmProvider::ClaudeBridge {
        Ok(configured)
    } else {
        Ok(None)
    }
}

fn parse_cli_bridge(
    object: &Map<String, Value>,
    provider: ProjectLlmProvider,
) -> Result<Option<CliBridgeConfig>, ProjectLlmConfigError> {
    if provider != ProjectLlmProvider::CliBridge {
        return Ok(None);
    }
    let ((Some(command), None) | (None, Some(command))) = (
        optional_string_value(object, "command")?,
        optional_string_value(object, "claudeBin")?,
    ) else {
        return Err(ProjectLlmConfigError::InvalidTier);
    };
    let args = object
        .get("args")
        .and_then(Value::as_array)
        .filter(|args| args.len() <= MAXIMUM_CLI_ARGUMENTS)
        .ok_or(ProjectLlmConfigError::InvalidTier)?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(ProjectLlmConfigError::InvalidTier)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let input = optional_string_value(object, "input")?
        .and_then(CliBridgeInputMode::parse)
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    let response_format = optional_string_value(object, "responseFormat")?
        .and_then(CliBridgeResponseFormat::parse)
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    let response_path = optional_string_value(object, "responsePath")?.map(str::to_owned);
    let mut config = CliBridgeConfig::new(
        CliBridgeConfigInput::new(command, input, response_format)
            .with_args(args)
            .with_response_path(response_path),
    )?;
    if let Some(prompt_template) = optional_string_value(object, "promptTemplate")? {
        config = config.with_prompt_template(prompt_template)?;
    }
    Ok(Some(config))
}

fn parse_llama_server_args(
    object: &Map<String, Value>,
) -> Result<Vec<String>, ProjectLlmConfigError> {
    let Some(value) = object.get("llamaServerArgs") else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .filter(|values| values.len() <= MAXIMUM_LLAMA_SERVER_ARGUMENTS)
        .ok_or(ProjectLlmConfigError::InvalidTier)?;
    let mut total = 0_usize;
    let mut arguments = Vec::with_capacity(values.len());
    for value in values {
        let argument = value
            .as_str()
            .filter(|argument| {
                !argument.is_empty()
                    && argument.len() <= MAXIMUM_LLAMA_SERVER_ARGUMENT_BYTES
                    && !argument.chars().any(char::is_control)
            })
            .ok_or(ProjectLlmConfigError::InvalidTier)?;
        total = total
            .checked_add(argument.len())
            .filter(|total| *total <= MAXIMUM_LLAMA_SERVER_ARGUMENT_TOTAL_BYTES)
            .ok_or(ProjectLlmConfigError::InvalidTier)?;
        arguments.push(argument.to_owned());
    }
    Ok(arguments)
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, ProjectLlmConfigError> {
    object
        .get(key)
        .map(|value| value.as_bool().ok_or(ProjectLlmConfigError::InvalidTier))
        .transpose()
}

fn validate_endpoint(raw: &str) -> Result<(), ProjectLlmConfigError> {
    crate::endpoint::validate_endpoint(raw).map_err(|()| ProjectLlmConfigError::InvalidTier)
}

fn validate_model(value: &str) -> Result<(), ProjectLlmConfigError> {
    if value.trim().is_empty()
        || value.len() > MAXIMUM_MODEL_BYTES
        || value.chars().any(char::is_control)
    {
        Err(ProjectLlmConfigError::InvalidTier)
    } else {
        Ok(())
    }
}

fn validate_api_key(value: &str) -> Result<(), ProjectLlmConfigError> {
    if value.is_empty()
        || value.len() > MAXIMUM_API_KEY_BYTES
        || value.chars().any(char::is_control)
    {
        Err(ProjectLlmConfigError::InvalidTier)
    } else {
        Ok(())
    }
}

fn validate_env_name(value: &str) -> Result<(), ProjectLlmConfigError> {
    let mut bytes = value.bytes();
    let first = bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_uppercase());
    if value.len() > 128
        || !first
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        Err(ProjectLlmConfigError::InvalidTier)
    } else {
        Ok(())
    }
}

fn object_field<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, ProjectLlmConfigError> {
    if !parent.contains_key(key) || parent.get(key).is_some_and(Value::is_null) {
        parent.insert(key.to_owned(), Value::Object(Map::new()));
    }
    parent
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or(ProjectLlmConfigError::InvalidConfig)
}

impl From<cartograph_config::ProjectConfigError> for ProjectLlmConfigError {
    fn from(error: cartograph_config::ProjectConfigError) -> Self {
        use cartograph_config::ProjectConfigError as Config;
        match error {
            Config::ProjectUnavailable => Self::ProjectUnavailable,
            Config::ConfigTooLarge => Self::ConfigTooLarge,
            Config::InvalidConfig => Self::InvalidConfig,
            Config::NumericFieldOutOfRange {
                field,
                minimum,
                maximum,
            } => Self::NumericFieldOutOfRange {
                field,
                minimum,
                maximum,
            },
            Config::WriteFailed => Self::WriteFailed,
            Config::ConcurrentModification => Self::ConcurrentModification,
        }
    }
}

/// Load neutral source policy through the configuration crate.
/// # Errors
/// Returns the compatible public error when source settings are invalid or unavailable.
pub fn load_project_source_settings(
    project_root: &Path,
) -> Result<ProjectSourceSettings, ProjectLlmConfigError> {
    cartograph_config::load_project_source_settings(project_root).map_err(Into::into)
}

/// Atomically update the source-file ceiling while preserving unrelated settings.
/// # Errors
/// Returns the compatible public error on invalid limits, unsafe I/O, or a concurrent edit.
pub fn write_project_max_file_size(
    project_root: &Path,
    max_file_size: usize,
) -> Result<(), ProjectLlmConfigError> {
    cartograph_config::write_project_max_file_size(project_root, max_file_size).map_err(Into::into)
}

fn read_config_value(project_root: &Path) -> Result<Option<Value>, ProjectLlmConfigError> {
    cartograph_config::read_project_config(project_root).map_err(Into::into)
}

fn read_config_snapshot(
    project_root: &Path,
) -> Result<Option<cartograph_config::ProjectConfigSnapshot>, ProjectLlmConfigError> {
    cartograph_config::read_project_config_snapshot(project_root).map_err(Into::into)
}

fn update_config_value<T>(
    project_root: &Path,
    update: impl FnOnce(Option<Value>) -> Result<(Value, T), ProjectLlmConfigError>,
) -> Result<T, ProjectLlmConfigError> {
    cartograph_config::update_project_config(project_root, update)
}

fn write_config_value_if_unchanged(
    project_root: &Path,
    value: &Value,
    expected: &[u8],
) -> Result<(), ProjectLlmConfigError> {
    cartograph_config::write_project_config_if_unchanged(project_root, value, expected)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    const CONFIG_DIRECTORY: &str = ".cartograph";
    const CONFIG_FILE: &str = "config.json";
    const MAXIMUM_PROJECT_SOURCE_BYTES: usize = 32 * 1024 * 1024;

    const LOCAL_SUMMARY_ENDPOINT: &str = "http://localhost:8081";
    const LEGACY_INLINE_CONFIG: &str = r#"{"llm":{"summarizeLlm":{"provider":"openai-compat","endpoint":"https://example.test","model":"fixture","apiKey":"do-not-print"}}}"#;
    const REJECTED_ENDPOINTS: [&str; 3] = [
        "http://example.test",
        "https://user:secret@example.test",
        "file:///tmp/model",
    ];
    const VALID_REMOTE_ENDPOINT: &str = "https://example.test";

    fn spawn_model_catalog_fixture() -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("model catalog listener failed: {error}"));
        let endpoint = format!(
            "http://{}/v1",
            listener
                .local_addr()
                .unwrap_or_else(|error| panic!("model catalog address failed: {error}"))
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("model catalog accept failed: {error}"));
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("model catalog timeout failed: {error}"));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1_024];
            while request.len() < 64 * 1_024 && !request.windows(4).any(|part| part == b"\r\n\r\n")
            {
                let read = stream
                    .read(&mut chunk)
                    .unwrap_or_else(|error| panic!("model catalog read failed: {error}"));
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8(request)
                .unwrap_or_else(|error| panic!("model catalog request was not UTF-8: {error}"));
            let (status, body) = if request.starts_with("GET /v1/models HTTP/1.1\r\n") {
                (
                    "200 OK",
                    r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
                )
            } else {
                ("404 Not Found", r#"{"error":"not found"}"#)
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .unwrap_or_else(|error| panic!("model catalog response failed: {error}"));
            request
        });
        (endpoint, server)
    }

    #[tokio::test]
    async fn endpoint_probe_reuses_existing_v1_base_for_model_catalog() {
        let (endpoint, server) = spawn_model_catalog_fixture();
        let probe = probe_openai_compatible_endpoint(&endpoint, Duration::from_secs(2))
            .await
            .unwrap_or_else(|error| panic!("model catalog probe failed: {error}"));
        assert!(probe.reachable);
        assert!(probe.openai_compatible);
        assert_eq!(probe.models, vec!["fixture-model"]);
        let request = server
            .join()
            .unwrap_or_else(|_| panic!("model catalog server panicked"));
        assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
    }

    #[test]
    fn project_config_round_trips_without_replacing_unrelated_fields() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"version":1,"languages":["rust"],"llm":{"enabled":true}}"#,
        )
        .unwrap_or_else(|error| panic!("config fixture failed: {error}"));
        let input = ProjectLlmTierInput::new(
            ProjectLlmTier::Summarize,
            LOCAL_SUMMARY_ENDPOINT,
            "fixture-chat",
        )
        .and_then(|input| input.with_concurrency(3))
        .map_or_else(
            |error| panic!("tier input failed: {error}"),
            ProjectLlmTierInput::without_credentials,
        );
        assert_eq!(input.tier(), ProjectLlmTier::Summarize);
        write_project_llm_tiers(root.path(), &[input])
            .unwrap_or_else(|error| panic!("tier write failed: {error}"));
        let loaded = load_project_llm_tier(root.path(), ProjectLlmTier::Summarize)
            .unwrap_or_else(|error| panic!("tier load failed: {error}"))
            .unwrap_or_else(|| panic!("tier missing"));
        assert_eq!(loaded.model(), "fixture-chat");
        assert_eq!(loaded.concurrency(), Some(3));
        assert_eq!(loaded.credential_source(), ProjectLlmCredentialSource::None);
        let value = read_config_value(root.path())
            .unwrap_or_else(|error| panic!("config reread failed: {error}"))
            .unwrap_or_else(|| panic!("config missing"));
        assert_eq!(value["languages"], json!(["rust"]));
        assert!(value["llm"]["summarizeLlm"].get("apiKey").is_none());
    }

    #[test]
    fn endpoint_origin_changes_clear_credentials_while_same_origin_updates_preserve_them() {
        let changed = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("origin-change tempdir failed: {error}"));
        fs::create_dir(changed.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("origin-change state failed: {error}"));
        fs::write(
            changed.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"version":2,"llm":{"enabled":true,"embeddingLlm":{"provider":"openai-compat","endpoint":"https://example.test/v1","model":"remote","apiKey":"fixture-secret"}}}"#,
        )
        .unwrap_or_else(|error| panic!("origin-change fixture failed: {error}"));
        let input = ProjectLlmTierInput::new(
            ProjectLlmTier::Embedding,
            "http://127.0.0.1:9999/v1",
            "local",
        )
        .unwrap_or_else(|error| panic!("origin-change input failed: {error}"));
        let report = write_project_llm_configuration_with_report(changed.path(), &[input], &[])
            .unwrap_or_else(|error| panic!("origin-change write failed: {error}"));
        assert_eq!(
            report.credential_actions,
            vec![ProjectLlmCredentialWriteEntry {
                tier: ProjectLlmTier::Embedding,
                action: ProjectLlmCredentialWriteAction::ClearedOriginChange,
            }]
        );
        let changed_value = read_config_value(changed.path())
            .unwrap_or_else(|error| panic!("origin-change read failed: {error}"))
            .unwrap_or_else(|| panic!("origin-change config missing"));
        assert!(changed_value["llm"]["embeddingLlm"].get("apiKey").is_none());
        assert!(
            changed_value["llm"]["embeddingLlm"]
                .get("apiKeyEnv")
                .is_none()
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap_or_else(|error| panic!("origin-change report failed: {error}"))
                .contains("fixture-secret")
        );

        let same = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("same-origin tempdir failed: {error}"));
        fs::create_dir(same.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("same-origin state failed: {error}"));
        fs::write(
            same.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"version":2,"llm":{"enabled":true,"embeddingLlm":{"provider":"openai-compat","endpoint":"https://example.test/v1","model":"old","apiKey":"fixture-secret"}}}"#,
        )
        .unwrap_or_else(|error| panic!("same-origin fixture failed: {error}"));
        let input =
            ProjectLlmTierInput::new(ProjectLlmTier::Embedding, "https://example.test/v2", "new")
                .unwrap_or_else(|error| panic!("same-origin input failed: {error}"));
        let report = write_project_llm_configuration_with_report(same.path(), &[input], &[])
            .unwrap_or_else(|error| panic!("same-origin write failed: {error}"));
        assert_eq!(
            report.credential_actions[0].action,
            ProjectLlmCredentialWriteAction::Preserved
        );
        let same_value = read_config_value(same.path())
            .unwrap_or_else(|error| panic!("same-origin read failed: {error}"))
            .unwrap_or_else(|| panic!("same-origin config missing"));
        assert_eq!(
            same_value["llm"]["embeddingLlm"]["apiKey"],
            Value::String("fixture-secret".to_owned())
        );
    }

    #[test]
    fn max_file_size_round_trips_without_replacing_llm_or_language_fields() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"version":2,"languages":["rust"],"llm":{"enabled":false}}"#,
        )
        .unwrap_or_else(|error| panic!("config fixture failed: {error}"));

        write_project_max_file_size(root.path(), 5 * 1024 * 1024)
            .unwrap_or_else(|error| panic!("max file size write failed: {error}"));
        assert_eq!(
            load_project_max_file_size(root.path()),
            Ok(Some(5 * 1024 * 1024))
        );
        let value = read_config_value(root.path())
            .unwrap_or_else(|error| panic!("config reread failed: {error}"))
            .unwrap_or_else(|| panic!("config missing"));
        assert_eq!(value["languages"], json!(["rust"]));
        assert_eq!(value["llm"]["enabled"], false);
        assert!(write_project_max_file_size(root.path(), 0).is_err());
        assert!(
            write_project_max_file_size(root.path(), MAXIMUM_PROJECT_SOURCE_BYTES + 1).is_err()
        );
    }

    #[test]
    fn loader_accepts_legacy_inline_secret_without_debug_disclosure() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            LEGACY_INLINE_CONFIG,
        )
        .unwrap_or_else(|error| panic!("legacy fixture failed: {error}"));
        let loaded = load_project_llm_tier(root.path(), ProjectLlmTier::Summarize)
            .unwrap_or_else(|error| panic!("legacy tier failed: {error}"))
            .unwrap_or_else(|| panic!("legacy tier missing"));
        assert_eq!(
            loaded.credential_source(),
            ProjectLlmCredentialSource::InlineLegacy
        );
        assert!(!format!("{loaded:?}").contains("do-not-print"));
    }

    #[test]
    fn inline_credential_migration_requires_an_exact_environment_match() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        let path = root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE);
        fs::write(&path, LEGACY_INLINE_CONFIG)
            .unwrap_or_else(|error| panic!("legacy fixture failed: {error}"));

        let blocked = migrate_project_inline_credentials_with(CredentialMigrationRequest {
            project_root: root.path(),
            environment_overrides: &[],
            apply: true,
            resolve: |_: &str| Some("different-secret".to_owned()),
        })
        .unwrap_or_else(|error| panic!("blocked migration failed: {error}"));
        assert_eq!(blocked.migrated, 0);
        assert_eq!(blocked.remaining_inline, 1);
        assert_eq!(
            blocked.candidates[0].status,
            ProjectCredentialMigrationStatus::EnvironmentMismatch
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("blocked config read failed: {error}"))
                .contains("do-not-print")
        );

        let dry_run = migrate_project_inline_credentials_with(CredentialMigrationRequest {
            project_root: root.path(),
            environment_overrides: &[(ProjectLlmTier::Summarize, "CARTOGRAPH_TEST_KEY".to_owned())],
            apply: false,
            resolve: |_: &str| Some("do-not-print".to_owned()),
        })
        .unwrap_or_else(|error| panic!("dry migration failed: {error}"));
        assert_eq!(
            dry_run.candidates[0].status,
            ProjectCredentialMigrationStatus::Ready
        );
        assert!(
            !serde_json::to_string(&dry_run)
                .unwrap_or_else(|error| panic!("report serialization failed: {error}"))
                .contains("do-not-print")
        );

        let applied = migrate_project_inline_credentials_with(CredentialMigrationRequest {
            project_root: root.path(),
            environment_overrides: &[(ProjectLlmTier::Summarize, "CARTOGRAPH_TEST_KEY".to_owned())],
            apply: true,
            resolve: |_: &str| Some("do-not-print".to_owned()),
        })
        .unwrap_or_else(|error| panic!("credential migration failed: {error}"));
        assert_eq!(applied.migrated, 1);
        assert_eq!(applied.remaining_inline, 0);
        let updated = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("updated config read failed: {error}"));
        assert!(!updated.contains("do-not-print"));
        assert!(updated.contains("\"apiKeyEnv\": \"CARTOGRAPH_TEST_KEY\""));
    }

    #[test]
    fn inline_credential_migration_rejects_a_concurrent_config_change() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        let path = root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE);
        fs::write(&path, LEGACY_INLINE_CONFIG)
            .unwrap_or_else(|error| panic!("legacy fixture failed: {error}"));
        let mut concurrent = serde_json::from_str::<Value>(LEGACY_INLINE_CONFIG)
            .unwrap_or_else(|error| panic!("concurrent fixture parse failed: {error}"));
        concurrent
            .as_object_mut()
            .unwrap_or_else(|| panic!("concurrent fixture root is not an object"))
            .insert("concurrentEdit".to_owned(), Value::Bool(true));
        let mut concurrent_bytes = serde_json::to_vec_pretty(&concurrent)
            .unwrap_or_else(|error| panic!("concurrent fixture encode failed: {error}"));
        concurrent_bytes.push(b'\n');
        let observed_path = path.clone();
        let observed_bytes = concurrent_bytes.clone();

        let result = migrate_project_inline_credentials_with_observer(
            CredentialMigrationRequest {
                project_root: root.path(),
                environment_overrides: &[(
                    ProjectLlmTier::Summarize,
                    "CARTOGRAPH_TEST_KEY".to_owned(),
                )],
                apply: true,
                resolve: |_: &str| Some("do-not-print".to_owned()),
            },
            move || {
                fs::write(observed_path, observed_bytes)
                    .unwrap_or_else(|error| panic!("concurrent config write failed: {error}"));
            },
        );

        assert_eq!(result, Err(ProjectLlmConfigError::ConcurrentModification));
        assert_eq!(
            fs::read(path).unwrap_or_else(|error| panic!("concurrent config read failed: {error}")),
            concurrent_bytes
        );
    }

    #[test]
    fn v1_chat_providers_defaults_and_ask_fallback_are_preserved() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"llm":{"summarizeLlm":{"provider":"claude-bridge","askModel":"claude-custom-ask","claudeBin":"/usr/bin/env","summaryBatchSize":9},"localLlm":{"provider":"anthropic-api","apiKey":"test-anthropic-key"}}}"#,
        )
        .unwrap_or_else(|error| panic!("provider fixture failed: {error}"));

        let summarize = load_exact_project_llm_tier(root.path(), ProjectLlmTier::Summarize)
            .unwrap_or_else(|error| panic!("summarize provider failed: {error}"))
            .unwrap_or_else(|| panic!("summarize provider missing"));
        assert_eq!(summarize.provider(), ProjectLlmProvider::ClaudeBridge);
        assert_eq!(summarize.endpoint(), CLAUDE_BRIDGE_ENDPOINT);
        assert_eq!(summarize.model(), DEFAULT_CLAUDE_SUMMARIZE_MODEL);
        assert_eq!(summarize.ask_model(), Some("claude-custom-ask"));
        assert_eq!(summarize.claude_bin(), Some("/usr/bin/env"));
        assert_eq!(summarize.summary_batch_size(), Some(9));

        let ask = load_project_llm_tier(root.path(), ProjectLlmTier::Ask)
            .unwrap_or_else(|error| panic!("ask fallback failed: {error}"))
            .unwrap_or_else(|| panic!("ask fallback missing"));
        assert_eq!(ask.provider(), ProjectLlmProvider::ClaudeBridge);
        assert_eq!(ask.model(), "claude-custom-ask");

        let local = load_exact_project_llm_tier(root.path(), ProjectLlmTier::Local)
            .unwrap_or_else(|error| panic!("anthropic provider failed: {error}"))
            .unwrap_or_else(|| panic!("anthropic provider missing"));
        assert_eq!(local.provider(), ProjectLlmProvider::AnthropicApi);
        assert_eq!(local.endpoint(), ANTHROPIC_CLOUD_ENDPOINT);
        assert_eq!(local.model(), DEFAULT_CLAUDE_ASK_MODEL);
        assert_eq!(
            local.credential_source(),
            ProjectLlmCredentialSource::InlineLegacy
        );

        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"llm":{"embeddingLlm":{"provider":"anthropic-api","model":"invalid"}}}"#,
        )
        .unwrap_or_else(|error| panic!("invalid provider fixture failed: {error}"));
        assert!(matches!(
            load_project_llm_tier(root.path(), ProjectLlmTier::Embedding),
            Err(ProjectLlmConfigError::InvalidTier)
        ));
    }

    #[test]
    fn generic_cli_bridge_round_trips_and_rejects_unsafe_templates() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let bridge = CliBridgeConfig::new(
            CliBridgeConfigInput::new(
                "some-agent-cli",
                CliBridgeInputMode::Arg,
                CliBridgeResponseFormat::Raw,
            )
            .with_args(
                ["-p", "{prompt}", "--model", "{model}"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            ),
        )
        .unwrap_or_else(|error| panic!("CLI bridge fixture failed: {error}"));
        let input = ProjectLlmTierInput::cli_bridge(
            ProjectLlmTier::Summarize,
            "some-model",
            bridge.clone(),
        )
        .unwrap_or_else(|error| panic!("CLI tier fixture failed: {error}"));
        write_project_llm_tiers(root.path(), &[input])
            .unwrap_or_else(|error| panic!("CLI tier write failed: {error}"));

        let value = read_config_value(root.path())
            .unwrap_or_else(|error| panic!("CLI config read failed: {error}"))
            .unwrap_or_else(|| panic!("CLI config was missing"));
        let tier = &value["llm"]["summarizeLlm"];
        assert_eq!(tier["provider"], "cli-bridge");
        assert_eq!(tier["command"], "some-agent-cli");
        assert_eq!(tier["input"], "arg");
        assert_eq!(tier["responseFormat"], "raw");
        assert!(tier.get("endpoint").is_none());
        assert!(tier.get("promptTemplate").is_none());

        let loaded = load_exact_project_llm_tier(root.path(), ProjectLlmTier::Summarize)
            .unwrap_or_else(|error| panic!("CLI tier load failed: {error}"))
            .unwrap_or_else(|| panic!("CLI tier was missing"));
        assert_eq!(loaded.provider(), ProjectLlmProvider::CliBridge);
        assert_eq!(loaded.endpoint(), CLI_BRIDGE_ENDPOINT);
        assert_eq!(loaded.cli_bridge(), Some(&bridge));
        assert_eq!(loaded.credential_source(), ProjectLlmCredentialSource::None);

        assert!(
            CliBridgeConfig::new(
                CliBridgeConfigInput::new(
                    "agent",
                    CliBridgeInputMode::Stdin,
                    CliBridgeResponseFormat::Raw,
                )
                .with_args(vec!["{unknown}".to_owned()]),
            )
            .is_err()
        );
        assert!(
            CliBridgeConfig::new(CliBridgeConfigInput::new(
                "agent",
                CliBridgeInputMode::Arg,
                CliBridgeResponseFormat::Raw,
            ),)
            .is_err()
        );
        assert!(
            CliBridgeConfig::new(
                CliBridgeConfigInput::new(
                    "agent",
                    CliBridgeInputMode::Stdin,
                    CliBridgeResponseFormat::JsonPath,
                )
                .with_response_path(Some(".messages[-1].content".to_owned())),
            )
            .is_ok()
        );
    }

    #[test]
    fn claude_compatible_cli_preset_preserves_the_historical_contract() {
        let bridge = CliBridgeConfig::claude_compatible(Some("/usr/bin/claude"))
            .unwrap_or_else(|error| panic!("Claude CLI preset failed: {error}"));
        assert_eq!(bridge.command(), "/usr/bin/claude");
        assert_eq!(
            bridge.args(),
            [
                "-p",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--model",
                "{model}",
                "--output-format",
                "json",
            ]
        );
        assert_eq!(bridge.input(), CliBridgeInputMode::Stdin);
        assert_eq!(bridge.prompt_template(), DEFAULT_CLI_PROMPT_TEMPLATE);
        assert_eq!(bridge.response_format(), CliBridgeResponseFormat::Claude);
        assert_eq!(bridge.response_path(), None);
    }

    #[test]
    fn summary_policy_preserves_fractional_negative_and_per_kind_v1_semantics() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"llm":{"enabled":true,"summarize":false,"summarizeEagerLimit":10.2,"minBodyLines":2.1,"minBodyLinesByKind":{"method":1.2}}}"#,
        )
        .unwrap_or_else(|error| panic!("summary policy fixture failed: {error}"));
        let settings = load_project_summary_settings(root.path())
            .unwrap_or_else(|error| panic!("summary settings failed: {error}"));
        assert!(!settings.enabled());
        assert_eq!(
            settings.eager_limit(),
            ProjectSummaryEagerLimit::Bounded(11)
        );
        assert_eq!(settings.minimum_body_lines(), 3);
        assert_eq!(settings.minimum_body_lines_by_kind().get("route"), Some(&1));
        assert_eq!(
            settings.minimum_body_lines_by_kind().get("method"),
            Some(&2)
        );

        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"llm":{"summarizeEagerLimit":-0.5}}"#,
        )
        .unwrap_or_else(|error| panic!("uncapped summary fixture failed: {error}"));
        assert_eq!(
            load_project_summary_settings(root.path())
                .unwrap_or_else(|error| panic!("uncapped summary settings failed: {error}"))
                .eager_limit(),
            ProjectSummaryEagerLimit::Uncapped
        );

        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"llm":{"minBodyLinesByKind":{"method":-1}}}"#,
        )
        .unwrap_or_else(|error| panic!("invalid summary fixture failed: {error}"));
        assert!(matches!(
            load_project_summary_settings(root.path()),
            Err(ProjectLlmConfigError::InvalidConfig)
        ));
    }

    #[test]
    fn endpoint_and_environment_name_policy_rejects_unsafe_values() {
        for endpoint in REJECTED_ENDPOINTS {
            assert!(ProjectLlmTierInput::new(ProjectLlmTier::Ask, endpoint, "model").is_err());
        }
        assert!(
            ProjectLlmTierInput::new(ProjectLlmTier::Ask, VALID_REMOTE_ENDPOINT, "model")
                .and_then(|input| input.with_api_key_env("bad-name"))
                .is_err()
        );
    }
}
