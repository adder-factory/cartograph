use std::{
    collections::BTreeMap,
    env, fmt,
    io::{self, IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use cartograph_config::{DATABASE_URL_ENV, DatabaseSettings};
use cartograph_db::{CartographDatabase, ManagedDatabase};
use cartograph_llm::{
    ChatMessageRequest, ChatSettings, CliBridgeConfig, CliBridgeConfigInput, CliBridgeInputMode,
    CliBridgeResponseFormat, CredentialCommand, EmbeddingSettings, InstallModelsOptions,
    OpenAiChatClient, OpenAiEmbeddingClient, OpenAiRerankClient, ProjectCredentialMigrationReport,
    ProjectCredentialMigrationStatus, ProjectLlmCredentialWriteEntry, ProjectLlmTier,
    ProjectLlmTierInput, RerankSettings, install_recommended_models, load_exact_project_llm_tier,
    migrate_project_inline_credentials, probe_openai_compatible_endpoint,
    write_project_llm_configuration, write_project_llm_configuration_with_report,
};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAXIMUM_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_MODEL_CONCURRENCY: u16 = 2;
const LLAMA_EMBED_ENDPOINT: &str = "http://127.0.0.1:8080";
const LLAMA_CHAT_ENDPOINT: &str = "http://127.0.0.1:8081";
const LLAMA_ASK_ENDPOINT: &str = "http://127.0.0.1:8082";
const LLAMA_RERANK_ENDPOINT: &str = "http://127.0.0.1:8083";
const OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";
const VLLM_ENDPOINT: &str = "http://127.0.0.1:8000";
const LM_STUDIO_ENDPOINT: &str = "http://127.0.0.1:1234";
#[cfg(test)]
const UNREACHABLE_LOOPBACK_ENDPOINT: &str = "http://127.0.0.1:1";
const OPENAI_ENDPOINT: &str = "https://api.openai.com";
const OPENAI_DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const JEV_DEFAULT_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
const CREDENTIAL_MIGRATION_CONFIRMATION: &str = "migrate-inline-credentials";
#[cfg(test)]
const OPENAI_V1_ENDPOINT: &str = "https://api.openai.com/v1";

#[derive(Debug, Subcommand)]
pub(super) enum LlmCommand {
    /// Select detected/local/cloud providers and atomically update config.json.
    Setup(Box<SetupArguments>),
    /// Send small real requests to every required or explicitly configured tier.
    Smoke(SmokeArguments),
    /// Download checksum-pinned recommended GGUFs and write the local stack config.
    Install(InstallArguments),
    /// Safely replace legacy inline API keys with environment references.
    MigrateCredentials(MigrateCredentialsArguments),
}

#[derive(Debug, Args)]
pub(super) struct SetupArguments {
    /// Existing project root.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Apply a preset non-interactively; omit for the interactive detector.
    #[arg(long, value_enum)]
    preset: Option<SetupPreset>,
    /// Custom preset tier.
    #[arg(long, value_enum)]
    tier: Option<LlmTierArgument>,
    /// Custom OpenAI-compatible base or complete API endpoint.
    #[arg(long)]
    endpoint: Option<String>,
    /// Custom provider model identifier.
    #[arg(long)]
    model: Option<String>,
    /// Generic CLI bridge executable passed directly without a shell.
    #[arg(long)]
    command: Option<String>,
    /// One generic CLI bridge argv template; repeat for each argument.
    #[arg(long = "arg")]
    command_args: Vec<String>,
    /// Generic CLI bridge prompt-delivery mode.
    #[arg(long, value_enum)]
    input: Option<CliBridgeInputArgument>,
    /// Optional generic CLI bridge system/user prompt template.
    #[arg(long)]
    prompt_template: Option<String>,
    /// Generic CLI bridge stdout decoder.
    #[arg(long, value_enum)]
    response_format: Option<CliBridgeResponseArgument>,
    /// JSON path required by the generic CLI bridge json-path decoder.
    #[arg(long)]
    response_path: Option<String>,
    #[command(flatten)]
    credentials: SetupCredentialArguments,
    /// Jev preset only: Cartograph features allowed to consult Jev
    /// (`explore` sends bounded source; `context` sends only candidate metadata;
    /// `roles` sends symbol metadata with signatures up to 160 bytes; `rename`
    /// sends each mention's path, line, enclosing symbol and up to 200 bytes of
    /// its source line).
    #[arg(long, value_enum, value_delimiter = ',')]
    jev_features: Option<Vec<JevFeatureArgument>>,
    /// Omit the 7B ask tier and reranker from local presets.
    #[arg(long)]
    minimal: bool,
    /// Apply the detected recommendation without prompting.
    #[arg(long)]
    yes: bool,
    /// Print structured JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct SetupCredentialArguments {
    /// Environment-variable name containing the provider credential.
    #[arg(long, conflicts_with = "api_key_command")]
    api_key_env: Option<String>,
    /// Credential helper the serving process runs without a shell on the
    /// tier's first use; its standard output (trailing whitespace trimmed) is
    /// the credential. Only the command line is stored.
    #[arg(long, value_name = "EXE")]
    api_key_command: Option<String>,
    /// One credential helper argument; repeat for each argument.
    #[arg(
        long = "api-key-arg",
        value_name = "ARG",
        requires = "api_key_command",
        allow_hyphen_values = true
    )]
    api_key_args: Vec<String>,
    /// Remove existing credentials; mutually exclusive with --api-key-env and
    /// --api-key-command.
    #[arg(long, conflicts_with_all = ["api_key_env", "api_key_command"])]
    clear_credentials: bool,
}

/// Counts the credential helper's arguments instead of printing them: an
/// argument can name or carry the secret itself.
impl fmt::Debug for SetupCredentialArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetupCredentialArguments")
            .field("api_key_env", &self.api_key_env)
            .field("api_key_command", &self.api_key_command)
            .field("api_key_argument_count", &self.api_key_args.len())
            .field("clear_credentials", &self.clear_credentials)
            .finish()
    }
}

impl SetupCredentialArguments {
    /// Whether any credential source or removal was requested.
    const fn requested(&self) -> bool {
        self.api_key_env.is_some() || self.api_key_command.is_some() || self.clear_credentials
    }

    /// The same exclusions clap enforces, for arguments built without clap.
    fn validate(&self) -> Result<(), String> {
        let sources = [
            self.api_key_env.is_some(),
            self.api_key_command.is_some(),
            self.clear_credentials,
        ]
        .into_iter()
        .filter(|requested| *requested)
        .count();
        if sources > 1 {
            Err(
                "--api-key-env, --api-key-command and --clear-credentials are mutually exclusive"
                    .to_owned(),
            )
        } else if self.api_key_command.is_none() && !self.api_key_args.is_empty() {
            Err("--api-key-arg requires --api-key-command".to_owned())
        } else {
            Ok(())
        }
    }

    /// Validated credential command from --api-key-command and --api-key-arg.
    fn command(&self) -> Result<Option<CredentialCommand>, String> {
        self.api_key_command
            .as_ref()
            .map(|program| {
                CredentialCommand::new(
                    std::iter::once(program.clone())
                        .chain(self.api_key_args.iter().cloned())
                        .collect(),
                )
                .map_err(|error| error.to_string())
            })
            .transpose()
    }

    /// Apply the requested credential source to one tier input. Without a
    /// request, `default_environment` names the variable to reference, if any.
    fn apply(
        &self,
        input: ProjectLlmTierInput,
        default_environment: Option<&str>,
    ) -> Result<ProjectLlmTierInput, String> {
        self.validate()?;
        if let Some(command) = self.command()? {
            return input
                .with_api_key_command(command)
                .map_err(|error| error.to_string());
        }
        match self.api_key_env.as_deref().or(default_environment) {
            Some(name) => input
                .with_api_key_env(name)
                .map_err(|error| error.to_string()),
            None if self.clear_credentials => Ok(input.without_credentials()),
            None => Ok(input),
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct SmokeArguments {
    /// Existing project root.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Per-tier request deadline in milliseconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_MS, value_parser = clap::value_parser!(u64).range(1..=MAXIMUM_TIMEOUT_MS))]
    timeout_ms: u64,
    /// Print structured JSON instead of Markdown.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(super) struct InstallArguments {
    /// Existing project root.
    #[arg(default_value = ".")]
    path: PathBuf,
    #[command(flatten)]
    models: ModelInstallFlags,
    /// Model directory (defaults to ~/.cartograph/models).
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Parallel checksum-bounded model downloads.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=4))]
    concurrency: u16,
    /// Storage provider compatibility selector. Cartograph v2 accepts PostgreSQL only.
    #[arg(long)]
    database_provider: Option<String>,
    /// Secret-bearing PostgreSQL URL used only by this process and never persisted.
    #[arg(long)]
    database_url: Option<String>,
    /// Validated PostgreSQL schema for this project.
    #[arg(long)]
    database_schema: Option<String>,
    /// pgvector compatibility selector. V2 requires auto or require.
    #[arg(long)]
    database_pgvector: Option<String>,
    /// Bounded PostgreSQL connection-pool size.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=64))]
    database_max_connections: Option<u32>,
    /// PostgreSQL statement timeout in milliseconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=600_000))]
    database_query_timeout_ms: Option<u64>,
    /// PostgreSQL connection acquisition timeout in seconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=120))]
    database_connection_timeout_seconds: Option<u64>,
    /// Require TLS at the PostgreSQL driver boundary.
    #[arg(long)]
    database_ssl: bool,
    /// Print structured JSON.
    #[arg(long)]
    json: bool,
}

/// Redacts the secret-bearing `--database-url`; every other field prints.
impl fmt::Debug for InstallArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstallArguments")
            .field("path", &self.path)
            .field("models", &self.models)
            .field("dir", &self.dir)
            .field("concurrency", &self.concurrency)
            .field("database_provider", &self.database_provider)
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "<redacted>"),
            )
            .field("database_schema", &self.database_schema)
            .field("database_pgvector", &self.database_pgvector)
            .field("database_max_connections", &self.database_max_connections)
            .field("database_query_timeout_ms", &self.database_query_timeout_ms)
            .field(
                "database_connection_timeout_seconds",
                &self.database_connection_timeout_seconds,
            )
            .field("database_ssl", &self.database_ssl)
            .field("json", &self.json)
            .finish()
    }
}

#[derive(Debug, Args)]
struct ModelInstallFlags {
    /// Install only embedding and 3B chat models.
    #[arg(long)]
    minimal: bool,
    /// Skip model downloads; useful when another provider manages its models.
    #[arg(long)]
    no_models: bool,
}

#[derive(Debug, Args)]
pub(super) struct MigrateCredentialsArguments {
    /// Existing project root.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Override a tier's target variable, for example `summarize=MY_API_KEY`.
    #[arg(long = "tier-env", value_parser = parse_tier_environment_override)]
    tier_environments: Vec<TierEnvironmentOverride>,
    /// Atomically rewrite credentials whose environment values match exactly.
    #[arg(long)]
    apply: bool,
    /// Exact phrase required with --apply: migrate-inline-credentials.
    #[arg(long, requires = "apply")]
    confirm: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug)]
struct TierEnvironmentOverride {
    tier: ProjectLlmTier,
    environment: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum SetupPreset {
    Jev,
    #[value(name = "install-llama-cpp", alias = "local-llama-cpp")]
    LocalLlamaCpp,
    #[value(name = "install-ollama", alias = "ollama")]
    Ollama,
    InstallMlx,
    CloudOpenAi,
    CloudOpenAiCompat,
    CliBridge,
    HybridClaudeBridge,
    HybridAnthropicApi,
    Custom,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum LlmTierArgument {
    Embed,
    Chat,
    Local,
    Ask,
    Classify,
    Reranker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliBridgeInputArgument {
    Stdin,
    Arg,
}

impl From<CliBridgeInputArgument> for CliBridgeInputMode {
    fn from(value: CliBridgeInputArgument) -> Self {
        match value {
            CliBridgeInputArgument::Stdin => Self::Stdin,
            CliBridgeInputArgument::Arg => Self::Arg,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliBridgeResponseArgument {
    Raw,
    JsonPath,
    Claude,
}

impl From<CliBridgeResponseArgument> for CliBridgeResponseFormat {
    fn from(value: CliBridgeResponseArgument) -> Self {
        match value {
            CliBridgeResponseArgument::Raw => Self::Raw,
            CliBridgeResponseArgument::JsonPath => Self::JsonPath,
            CliBridgeResponseArgument::Claude => Self::Claude,
        }
    }
}

impl From<LlmTierArgument> for ProjectLlmTier {
    fn from(value: LlmTierArgument) -> Self {
        match value {
            LlmTierArgument::Embed => Self::Embedding,
            LlmTierArgument::Chat => Self::Summarize,
            LlmTierArgument::Local => Self::Local,
            LlmTierArgument::Ask => Self::Ask,
            LlmTierArgument::Classify => Self::Classify,
            LlmTierArgument::Reranker => Self::Reranker,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupReport {
    applied: bool,
    preset: SetupPreset,
    detected: Vec<DetectedEndpoint>,
    configured_tiers: Vec<ProjectLlmTier>,
    cleared_tiers: Vec<ProjectLlmTier>,
    credential_actions: Vec<ProjectLlmCredentialWriteEntry>,
    next_steps: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectedEndpoint {
    endpoint: String,
    reachable: bool,
    openai_compatible: bool,
    models: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SmokeStatus {
    Ok,
    Skip,
    Fail,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SmokeRow {
    tier: &'static str,
    status: SmokeStatus,
    model: Option<String>,
    endpoint: Option<String>,
    duration_ms: u64,
    detail: String,
}

struct SmokeRowInput {
    tier: &'static str,
    model: Option<String>,
    endpoint: Option<String>,
    started: Instant,
    detail: String,
}

impl SmokeRowInput {
    fn new(tier: &'static str, started: Instant, detail: impl Into<String>) -> Self {
        Self {
            tier,
            model: None,
            endpoint: None,
            started,
            detail: detail.into(),
        }
    }

    fn with_configuration(mut self, model: Option<String>, endpoint: Option<String>) -> Self {
        self.model = model;
        self.endpoint = endpoint;
        self
    }

    fn into_row(self, status: SmokeStatus) -> SmokeRow {
        SmokeRow {
            tier: self.tier,
            status,
            model: self.model,
            endpoint: self.endpoint,
            duration_ms: elapsed_ms(self.started),
            detail: self.detail,
        }
    }
}

struct TierInputRequest<'endpoint> {
    tier: ProjectLlmTier,
    endpoint: &'endpoint str,
    model: String,
    concurrency: u16,
}

impl<'endpoint> TierInputRequest<'endpoint> {
    fn new(tier: ProjectLlmTier, endpoint: &'endpoint str, model: impl Into<String>) -> Self {
        Self {
            tier,
            endpoint,
            model: model.into(),
            concurrency: DEFAULT_MODEL_CONCURRENCY,
        }
    }

    const fn with_concurrency(mut self, concurrency: u16) -> Self {
        self.concurrency = concurrency;
        self
    }

    fn build(self) -> Result<ProjectLlmTierInput, String> {
        ProjectLlmTierInput::new(self.tier, self.endpoint, self.model)
            .and_then(|input| input.with_concurrency(self.concurrency))
            .map(ProjectLlmTierInput::without_credentials)
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy)]
struct ChatSmokeTarget {
    tier: ProjectLlmTier,
    label: &'static str,
    required: bool,
}

struct ChatSmokeRequest<'project> {
    project: &'project Path,
    target: ChatSmokeTarget,
    timeout: Duration,
}

impl<'project> ChatSmokeRequest<'project> {
    const fn new(project: &'project Path, target: ChatSmokeTarget, timeout: Duration) -> Self {
        Self {
            project,
            target,
            timeout,
        }
    }
}

const SUMMARIZE_SMOKE: ChatSmokeTarget = ChatSmokeTarget {
    tier: ProjectLlmTier::Summarize,
    label: "summarize",
    required: false,
};
const ASK_SMOKE: ChatSmokeTarget = ChatSmokeTarget {
    tier: ProjectLlmTier::Ask,
    label: "ask",
    required: false,
};
const LOCAL_SMOKE: ChatSmokeTarget = ChatSmokeTarget {
    tier: ProjectLlmTier::Local,
    label: "local",
    required: false,
};
const CLASSIFY_SMOKE: ChatSmokeTarget = ChatSmokeTarget {
    tier: ProjectLlmTier::Classify,
    label: "classify",
    required: false,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OverallSmokeStatus {
    Ok,
    Fail,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SmokeReport {
    overall_status: OverallSmokeStatus,
    rows: Vec<SmokeRow>,
    duration_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallReport {
    models: Option<cartograph_llm::InstallModelsReport>,
    config_written: bool,
    minimal: bool,
    database: &'static str,
    database_migrations_applied: usize,
    doctor: crate::DoctorReport,
    next_steps: Vec<String>,
}

pub(super) async fn run(command: LlmCommand) -> Result<ExitCode, String> {
    match command {
        LlmCommand::Setup(arguments) => run_setup(*arguments).await,
        LlmCommand::Smoke(arguments) => run_smoke(arguments).await,
        LlmCommand::Install(arguments) => run_install(arguments).await,
        LlmCommand::MigrateCredentials(arguments) => run_migrate_credentials(arguments),
    }
}

fn parse_tier_environment_override(raw: &str) -> Result<TierEnvironmentOverride, String> {
    let (tier, environment) = raw
        .split_once('=')
        .ok_or_else(|| "tier environment must use tier=ENVIRONMENT".to_owned())?;
    let tier = match tier {
        "embed" | "embedding" => ProjectLlmTier::Embedding,
        "chat" | "summarize" => ProjectLlmTier::Summarize,
        "local" => ProjectLlmTier::Local,
        "ask" => ProjectLlmTier::Ask,
        "classify" => ProjectLlmTier::Classify,
        "rerank" | "reranker" => ProjectLlmTier::Reranker,
        "decision" | "jev" => ProjectLlmTier::Decision,
        _ => return Err("unknown LLM tier in --tier-env".to_owned()),
    };
    if environment.is_empty() {
        return Err("tier environment name cannot be empty".to_owned());
    }
    Ok(TierEnvironmentOverride {
        tier,
        environment: environment.to_owned(),
    })
}

fn run_migrate_credentials(arguments: MigrateCredentialsArguments) -> Result<ExitCode, String> {
    if arguments.apply && arguments.confirm.as_deref() != Some(CREDENTIAL_MIGRATION_CONFIRMATION) {
        return Err(format!(
            "llm migrate-credentials --apply requires --confirm {CREDENTIAL_MIGRATION_CONFIRMATION}"
        ));
    }
    let overrides = arguments
        .tier_environments
        .into_iter()
        .map(|override_| (override_.tier, override_.environment))
        .collect::<Vec<_>>();
    let report = migrate_project_inline_credentials(&arguments.path, &overrides, arguments.apply)
        .map_err(|error| error.to_string())?;
    let blocked = report.candidates.iter().any(|entry| {
        matches!(
            entry.status,
            ProjectCredentialMigrationStatus::EnvironmentMissing
                | ProjectCredentialMigrationStatus::EnvironmentMismatch
                | ProjectCredentialMigrationStatus::UnsupportedProvider
        )
    });
    if arguments.json {
        print_json(&report)?;
    } else {
        render_credential_migration(&report);
    }
    Ok(if blocked {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn render_credential_migration(report: &ProjectCredentialMigrationReport) {
    println!("## cartograph LLM credential migration\n");
    if report.dry_run {
        println!("_Dry run: config.json was not changed._\n");
    }
    if report.candidates.is_empty() {
        println!("No legacy inline tier credentials were found.");
        return;
    }
    for entry in &report.candidates {
        println!(
            "- {:?}: {:?} via {}",
            entry.tier,
            entry.status,
            entry
                .environment
                .as_deref()
                .unwrap_or("no supported variable")
        );
    }
    println!(
        "\nMigrated {}; {} inline credential(s) remain.",
        report.migrated, report.remaining_inline
    );
}

/// Apply only the missing embedding tier for `doctor --fix`.
///
/// Summarization, chat, classification, and reranking remain optional. Existing
/// tier choices and credentials are never replaced. A detected Ollama endpoint
/// is reused; otherwise the checksum-pinned minimal llama.cpp embedding model is
/// installed and configured. Starting provider processes remains an explicit
/// operator action.
pub(super) async fn doctor_fix_missing_tiers(project: &Path) -> Result<Vec<String>, String> {
    let required = [ProjectLlmTier::Embedding];
    let mut missing = Vec::new();
    for tier in required {
        match load_exact_project_llm_tier(project, tier) {
            Ok(Some(_)) => {}
            Ok(None) => missing.push(tier),
            Err(error) => return Err(error.to_string()),
        }
    }
    if missing.is_empty() {
        return Ok(Vec::new());
    }

    let detected = detect_endpoints().await;
    let preset = recommend_preset(&detected);
    let (recommended, source) = match preset {
        SetupPreset::Ollama => (ollama_inputs(true)?.0, "detected Ollama".to_owned()),
        SetupPreset::LocalLlamaCpp => {
            let directory = default_models_directory()?;
            install_recommended_models(
                InstallModelsOptions::new(directory.clone(), true, DEFAULT_MODEL_CONCURRENCY)
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
            (
                local_inputs_in(&directory, true)?.0,
                "checksum-pinned minimal llama.cpp models".to_owned(),
            )
        }
        SetupPreset::InstallMlx
        | SetupPreset::CloudOpenAi
        | SetupPreset::CloudOpenAiCompat
        | SetupPreset::CliBridge
        | SetupPreset::HybridClaudeBridge
        | SetupPreset::HybridAnthropicApi
        | SetupPreset::Custom
        | SetupPreset::Jev
        | SetupPreset::Skip => {
            return Err("doctor could not select an automatic LLM repair preset".to_owned());
        }
    };
    let inputs = recommended
        .into_iter()
        .filter(|input| missing.contains(&input.tier()))
        .collect::<Vec<_>>();
    write_project_llm_configuration(project, &inputs, &[]).map_err(|error| error.to_string())?;
    Ok(vec![format!(
        "configured {} missing required LLM tier(s) from {source}",
        inputs.len()
    )])
}

async fn run_setup(arguments: SetupArguments) -> Result<ExitCode, String> {
    let detected = detect_endpoints().await;
    let recommended = recommend_preset(&detected);
    let preset = match arguments.preset {
        Some(preset) => preset,
        None if arguments.yes => recommended,
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => {
            prompt_preset(recommended)?
        }
        None => {
            render_detection(&detected, recommended);
            return Err(
                "non-interactive setup requires --preset (or --yes to apply the detected recommendation)"
                    .to_owned(),
            );
        }
    };
    let (inputs, cleared) = setup_inputs(&arguments, preset)?;
    let applied = preset != SetupPreset::Skip;
    let credential_actions = if applied {
        write_project_llm_configuration_with_report(&arguments.path, &inputs, &cleared)
            .map_err(|error| error.to_string())?
            .credential_actions
    } else {
        Vec::new()
    };
    let report = SetupReport {
        applied,
        preset,
        detected,
        configured_tiers: inputs.iter().map(ProjectLlmTierInput::tier).collect(),
        cleared_tiers: cleared,
        credential_actions,
        next_steps: if applied {
            vec![
                "Start or restart the configured provider processes.".to_owned(),
                format!("Run `cartograph llm smoke {}`.", arguments.path.display()),
                format!("Run `cartograph doctor {}`.", arguments.path.display()),
            ]
        } else {
            vec![
                "Deterministic BM25 and graph retrieval remain available without an LLM."
                    .to_owned(),
            ]
        },
    };
    if arguments.json {
        print_json(&report)?;
    } else {
        println!(
            "LLM setup {} ({preset:?}); configured {} tier(s), cleared {}.",
            if applied { "applied" } else { "skipped" },
            report.configured_tiers.len(),
            report.cleared_tiers.len()
        );
        for step in &report.next_steps {
            println!("- {step}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn setup_inputs(
    arguments: &SetupArguments,
    preset: SetupPreset,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    if arguments.jev_features.is_some() && preset != SetupPreset::Jev {
        return Err("--jev-features applies only to --preset jev".to_owned());
    }
    match preset {
        SetupPreset::Jev => jev_inputs(arguments),
        SetupPreset::LocalLlamaCpp => {
            reject_custom_fields(arguments)?;
            local_inputs(arguments.minimal)
        }
        SetupPreset::Ollama => {
            reject_custom_fields(arguments)?;
            ollama_inputs(arguments.minimal)
        }
        SetupPreset::CliBridge => cli_bridge_inputs(arguments),
        SetupPreset::HybridClaudeBridge => hybrid_claude_inputs(arguments),
        SetupPreset::HybridAnthropicApi => hybrid_anthropic_inputs(arguments),
        SetupPreset::CloudOpenAi => cloud_openai_inputs(arguments),
        SetupPreset::InstallMlx | SetupPreset::CloudOpenAiCompat | SetupPreset::Custom => {
            custom_inputs(arguments)
        }
        SetupPreset::Skip => {
            reject_custom_fields(arguments)?;
            Ok((Vec::new(), Vec::new()))
        }
    }
}

/// Cartograph surfaces the Jev decision tier may serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(super) enum JevFeatureArgument {
    /// Decision-guided exploration.
    Explore,
    /// Metadata-only context ranking.
    Context,
    /// Metadata-only symbol role classification.
    Roles,
    /// Rename-mention triage over mention source lines.
    Rename,
}

impl JevFeatureArgument {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Context => "context",
            Self::Roles => "roles",
            Self::Rename => "rename",
        }
    }
}

fn jev_inputs(
    arguments: &SetupArguments,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    reject_cli_bridge_fields(arguments)?;
    if arguments.tier.is_some()
        || arguments.endpoint.is_some()
        || arguments.model.is_some()
        || arguments.minimal
    {
        return Err("Jev uses the decision tier and its pinned model/endpoint; configure only --api-key-env, --api-key-command or --clear-credentials".to_owned());
    }
    arguments.credentials.validate()?;
    if arguments.credentials.clear_credentials {
        if arguments.jev_features.is_some() {
            return Err("--clear-credentials conflicts with --jev-features".to_owned());
        }
        return Ok((Vec::new(), vec![ProjectLlmTier::Decision]));
    }
    let mut input = arguments.credentials.apply(
        ProjectLlmTierInput::jev(JEV_DEFAULT_API_KEY_ENV).map_err(|error| error.to_string())?,
        Some(JEV_DEFAULT_API_KEY_ENV),
    )?;
    if let Some(features) = &arguments.jev_features {
        let mut names = features
            .iter()
            .map(|feature| feature.as_str().to_owned())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        input = input
            .with_decision_features(names)
            .map_err(|error| error.to_string())?;
    }
    Ok((vec![input], Vec::new()))
}

fn reject_custom_fields(arguments: &SetupArguments) -> Result<(), String> {
    let provider_fields_present = [
        arguments.tier.is_some(),
        arguments.endpoint.is_some(),
        arguments.model.is_some(),
        has_cli_bridge_fields(arguments),
        arguments.credentials.requested(),
    ]
    .contains(&true);
    if provider_fields_present {
        Err("provider fields require --preset custom or --preset cli-bridge".to_owned())
    } else {
        Ok(())
    }
}

fn has_cli_bridge_fields(arguments: &SetupArguments) -> bool {
    arguments.command.is_some()
        || !arguments.command_args.is_empty()
        || arguments.input.is_some()
        || arguments.prompt_template.is_some()
        || arguments.response_format.is_some()
        || arguments.response_path.is_some()
}

fn reject_cli_bridge_fields(arguments: &SetupArguments) -> Result<(), String> {
    if has_cli_bridge_fields(arguments) {
        Err("CLI transport fields require --preset cli-bridge".to_owned())
    } else {
        Ok(())
    }
}

fn local_inputs(minimal: bool) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    let directory = default_models_directory()?;
    local_inputs_in(&directory, minimal)
}

fn ollama_inputs(minimal: bool) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    let endpoint = OLLAMA_ENDPOINT;
    let mut inputs = vec![
        TierInputRequest::new(ProjectLlmTier::Embedding, endpoint, "nomic-embed-text")
            .with_concurrency(4)
            .build()?,
        TierInputRequest::new(ProjectLlmTier::Summarize, endpoint, "qwen2.5-coder:3b").build()?,
        TierInputRequest::new(ProjectLlmTier::Local, endpoint, "qwen2.5-coder:3b").build()?,
        TierInputRequest::new(ProjectLlmTier::Classify, endpoint, "qwen2.5-coder:3b").build()?,
    ];
    if !minimal {
        inputs.push(
            TierInputRequest::new(ProjectLlmTier::Ask, endpoint, "qwen2.5-coder:7b")
                .with_concurrency(1)
                .build()?,
        );
    }
    Ok((
        inputs,
        vec![ProjectLlmTier::Reranker]
            .into_iter()
            .chain(minimal.then_some(ProjectLlmTier::Ask))
            .collect(),
    ))
}

fn custom_inputs(
    arguments: &SetupArguments,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    reject_cli_bridge_fields(arguments)?;
    if arguments.minimal {
        return Err("--minimal applies only to local-llama-cpp and ollama presets".to_owned());
    }
    let tier = arguments
        .tier
        .ok_or_else(|| "--preset custom requires --tier".to_owned())?;
    let endpoint = arguments
        .endpoint
        .as_deref()
        .ok_or_else(|| "--preset custom requires --endpoint".to_owned())?;
    let model = arguments
        .model
        .as_deref()
        .ok_or_else(|| "--preset custom requires --model".to_owned())?;
    let input = ProjectLlmTierInput::new(tier.into(), endpoint, model)
        .map_err(|error| error.to_string())?;
    let input = arguments.credentials.apply(input, None)?;
    Ok((vec![input], Vec::new()))
}

fn cli_bridge_inputs(
    arguments: &SetupArguments,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    if arguments.minimal || arguments.endpoint.is_some() || arguments.credentials.requested() {
        return Err(
            "cli-bridge accepts chat tier, model, and command transport fields without HTTP or credential options"
                .to_owned(),
        );
    }
    let tier = arguments
        .tier
        .ok_or_else(|| "--preset cli-bridge requires --tier".to_owned())?;
    let model = arguments
        .model
        .as_deref()
        .ok_or_else(|| "--preset cli-bridge requires --model".to_owned())?;
    let command = arguments
        .command
        .as_deref()
        .ok_or_else(|| "--preset cli-bridge requires --command".to_owned())?;
    let input = arguments
        .input
        .ok_or_else(|| "--preset cli-bridge requires --input".to_owned())?;
    let response_format = arguments
        .response_format
        .ok_or_else(|| "--preset cli-bridge requires --response-format".to_owned())?;
    let mut bridge = CliBridgeConfig::new(
        CliBridgeConfigInput::new(command, input.into(), response_format.into())
            .with_args(arguments.command_args.clone())
            .with_response_path(arguments.response_path.clone()),
    )
    .map_err(|error| error.to_string())?;
    if let Some(prompt_template) = &arguments.prompt_template {
        bridge = bridge
            .with_prompt_template(prompt_template)
            .map_err(|error| error.to_string())?;
    }
    let input = ProjectLlmTierInput::cli_bridge(tier.into(), model, bridge)
        .map_err(|error| error.to_string())?;
    Ok((vec![input], Vec::new()))
}

fn cloud_openai_inputs(
    arguments: &SetupArguments,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    reject_cli_bridge_fields(arguments)?;
    if arguments.minimal {
        return Err("--minimal applies only to local model presets".to_owned());
    }
    if arguments.credentials.clear_credentials {
        return Err("--clear-credentials requires --preset custom".to_owned());
    }
    let tier = arguments
        .tier
        .ok_or_else(|| "--preset cloud-open-ai requires --tier".to_owned())?;
    let model = arguments
        .model
        .as_deref()
        .ok_or_else(|| "--preset cloud-open-ai requires --model".to_owned())?;
    let input = ProjectLlmTierInput::new(
        tier.into(),
        arguments.endpoint.as_deref().unwrap_or(OPENAI_ENDPOINT),
        model,
    )
    .map_err(|error| error.to_string())?;
    let input = arguments
        .credentials
        .apply(input, Some(OPENAI_DEFAULT_API_KEY_ENV))?;
    Ok((vec![input], Vec::new()))
}

fn hybrid_claude_inputs(
    arguments: &SetupArguments,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    if hybrid_claude_has_custom_fields(arguments) {
        return Err(
            "hybrid-claude-bridge uses its bounded default tiers and accepts no custom tier fields"
                .to_owned(),
        );
    }
    let mut inputs = vec![hybrid_embedding_input()?];
    inputs.push(
        claude_cli_input(ProjectLlmTier::Summarize, "claude-haiku-4-5")
            .and_then(|input| input.with_ask_model("claude-sonnet-4-6"))
            .and_then(|input| input.with_summary_batch_size(3))
            .map_err(|error| error.to_string())?,
    );
    for (tier, model) in [
        (ProjectLlmTier::Local, "claude-sonnet-4-6"),
        (ProjectLlmTier::Ask, "claude-sonnet-4-6"),
        (ProjectLlmTier::Classify, "claude-haiku-4-5"),
    ] {
        inputs.push(claude_cli_input(tier, model).map_err(|error| error.to_string())?);
    }
    Ok((inputs, vec![ProjectLlmTier::Reranker]))
}

fn claude_cli_input(
    tier: ProjectLlmTier,
    model: &str,
) -> Result<ProjectLlmTierInput, cartograph_llm::ProjectLlmConfigError> {
    let bridge = CliBridgeConfig::claude_compatible(None)?;
    ProjectLlmTierInput::cli_bridge(tier, model, bridge)
}

fn hybrid_claude_has_custom_fields(arguments: &SetupArguments) -> bool {
    [
        arguments.minimal,
        arguments.tier.is_some(),
        arguments.endpoint.is_some(),
        arguments.model.is_some(),
        arguments.credentials.requested(),
        has_cli_bridge_fields(arguments),
    ]
    .contains(&true)
}

fn hybrid_anthropic_inputs(
    arguments: &SetupArguments,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    if hybrid_anthropic_has_custom_fields(arguments) {
        return Err(
            "hybrid-anthropic-api uses its bounded default chat tiers; only --api-key-env or --api-key-command may override credential lookup"
                .to_owned(),
        );
    }
    let make = |tier, model| -> Result<ProjectLlmTierInput, String> {
        let input =
            ProjectLlmTierInput::anthropic_api(tier, model).map_err(|error| error.to_string())?;
        arguments.credentials.apply(input, None)
    };
    let mut inputs = vec![hybrid_embedding_input()?];
    inputs.push(
        make(ProjectLlmTier::Summarize, "claude-haiku-4-5")?
            .with_ask_model("claude-sonnet-4-6")
            .and_then(|input| input.with_summary_batch_size(3))
            .map_err(|error| error.to_string())?,
    );
    inputs.push(make(ProjectLlmTier::Local, "claude-sonnet-4-6")?);
    inputs.push(make(ProjectLlmTier::Ask, "claude-sonnet-4-6")?);
    inputs.push(make(ProjectLlmTier::Classify, "claude-haiku-4-5")?);
    Ok((inputs, vec![ProjectLlmTier::Reranker]))
}

fn hybrid_anthropic_has_custom_fields(arguments: &SetupArguments) -> bool {
    [
        arguments.minimal,
        arguments.tier.is_some(),
        arguments.endpoint.is_some(),
        arguments.model.is_some(),
        arguments.credentials.clear_credentials,
        has_cli_bridge_fields(arguments),
    ]
    .contains(&true)
}

fn hybrid_embedding_input() -> Result<ProjectLlmTierInput, String> {
    let model = default_models_directory()?
        .join("jina-embeddings-v2-base-code.Q4_K_M.gguf")
        .to_string_lossy()
        .into_owned();
    TierInputRequest::new(ProjectLlmTier::Embedding, LLAMA_EMBED_ENDPOINT, model)
        .with_concurrency(4)
        .build()
}

async fn detect_endpoints() -> Vec<DetectedEndpoint> {
    let (llama_embed, llama_chat, ollama, mlx, studio) = tokio::join!(
        detect_one(LLAMA_EMBED_ENDPOINT),
        detect_one(LLAMA_CHAT_ENDPOINT),
        detect_one(OLLAMA_ENDPOINT),
        detect_one(VLLM_ENDPOINT),
        detect_one(LM_STUDIO_ENDPOINT),
    );
    vec![llama_embed, llama_chat, ollama, mlx, studio]
}

async fn detect_one(endpoint: &str) -> DetectedEndpoint {
    let probe = probe_openai_compatible_endpoint(endpoint, Duration::from_secs(2)).await;
    match probe {
        Ok(probe) => DetectedEndpoint {
            endpoint: endpoint.to_owned(),
            reachable: probe.reachable,
            openai_compatible: probe.openai_compatible,
            models: probe.models,
        },
        Err(_) => DetectedEndpoint {
            endpoint: endpoint.to_owned(),
            reachable: false,
            openai_compatible: false,
            models: Vec::new(),
        },
    }
}

fn recommend_preset(detected: &[DetectedEndpoint]) -> SetupPreset {
    if detected
        .iter()
        .any(|entry| entry.endpoint.ends_with(":11434") && entry.openai_compatible)
    {
        SetupPreset::Ollama
    } else {
        SetupPreset::LocalLlamaCpp
    }
}

fn prompt_preset(recommended: SetupPreset) -> Result<SetupPreset, String> {
    println!("Detected LLM providers. Choose a setup preset:");
    println!("  1) local-llama-cpp");
    println!("  2) ollama");
    println!("  3) hybrid-claude-bridge");
    println!("  4) hybrid-anthropic-api");
    println!("  5) cloud-open-ai (use non-interactive tier/model flags)");
    println!("  6) custom OpenAI-compatible (use non-interactive flags)");
    println!("  7) cli-bridge (use non-interactive command/argv flags)");
    println!("  8) skip");
    print!("Choice [{recommended:?}]: ");
    io::stdout()
        .flush()
        .map_err(|_| "could not prompt for LLM setup".to_owned())?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|_| "could not read LLM setup choice".to_owned())?;
    match answer.trim() {
        "" => Ok(recommended),
        "1" | "local-llama-cpp" => Ok(SetupPreset::LocalLlamaCpp),
        "2" | "ollama" => Ok(SetupPreset::Ollama),
        "3" | "hybrid-claude-bridge" => Ok(SetupPreset::HybridClaudeBridge),
        "4" | "hybrid-anthropic-api" => Ok(SetupPreset::HybridAnthropicApi),
        "5" | "cloud-open-ai" => Err(
            "cloud OpenAI setup requires --preset cloud-open-ai --tier <tier> --model <id>"
                .to_owned(),
        ),
        "6" | "custom" => Err(
            "custom setup requires --preset custom --tier <tier> --endpoint <url> --model <id>"
                .to_owned(),
        ),
        "7" | "cli-bridge" => Err(
            "CLI bridge setup requires --preset cli-bridge --tier <tier> --model <id> --command <binary> --input <stdin|arg> --response-format <raw|json-path|claude>"
                .to_owned(),
        ),
        "8" | "skip" => Ok(SetupPreset::Skip),
        _ => Err("LLM setup choice was not recognized".to_owned()),
    }
}

fn render_detection(detected: &[DetectedEndpoint], recommended: SetupPreset) {
    println!("LLM setup recommendation: {recommended:?}");
    for endpoint in detected.iter().filter(|entry| entry.reachable) {
        println!(
            "- {}: {} ({} model(s))",
            endpoint.endpoint,
            if endpoint.openai_compatible {
                "OpenAI-compatible"
            } else {
                "reachable but incompatible"
            },
            endpoint.models.len()
        );
    }
}

async fn run_smoke(arguments: SmokeArguments) -> Result<ExitCode, String> {
    let started = Instant::now();
    let timeout = Duration::from_millis(arguments.timeout_ms);
    let (embedding, summarize, ask, local, classify, rerank, decision) = tokio::join!(
        smoke_embedding(&arguments.path, timeout),
        smoke_chat(ChatSmokeRequest::new(
            &arguments.path,
            SUMMARIZE_SMOKE,
            timeout,
        )),
        smoke_chat(ChatSmokeRequest::new(&arguments.path, ASK_SMOKE, timeout,)),
        smoke_chat(ChatSmokeRequest::new(&arguments.path, LOCAL_SMOKE, timeout,)),
        smoke_chat(ChatSmokeRequest::new(
            &arguments.path,
            CLASSIFY_SMOKE,
            timeout,
        )),
        smoke_rerank(&arguments.path, timeout),
        smoke_jev(&arguments.path, timeout),
    );
    let rows = vec![embedding, summarize, ask, local, classify, rerank, decision];
    let overall_status = overall_smoke_status(&rows);
    let report = SmokeReport {
        overall_status,
        rows,
        duration_ms: elapsed_ms(started),
    };
    if arguments.json {
        print_json(&report)?;
    } else {
        render_smoke(&report);
    }
    Ok(if overall_status == OverallSmokeStatus::Fail {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn smoke_jev(project: &Path, timeout: Duration) -> SmokeRow {
    use cartograph_llm::{JevAnswer, JevClient, JevQuestion, JevSettings};
    let started = Instant::now();
    let config = match load_exact_project_llm_tier(project, ProjectLlmTier::Decision) {
        Ok(Some(config)) => config,
        Ok(None) => return missing_row("decision", false, started),
        Err(error) => {
            return failed_row(SmokeRowInput::new("decision", started, error.to_string()));
        }
    };
    let model = Some(config.model().to_owned());
    let endpoint = Some(config.endpoint().to_owned());
    let result = async {
        let settings = JevSettings::from_config(&config).map_err(|error| error.to_string())?;
        let client = JevClient::new(settings).map_err(|error| error.to_string())?;
        let questions = BTreeMap::from([(
            "ready".to_owned(),
            JevQuestion::Noul {
                instructions: "Is the supplied probe status ready?".to_owned(),
                criteria: None,
            },
        )]);
        let decision = client
            .decide(&serde_json::json!({"status":"ready"}), &questions)
            .await
            .map_err(|error| error.to_string())?;
        if !matches!(decision.answers.get("ready"), Some(JevAnswer::Noul { noul }) if *noul >= 0.5)
        {
            return Err("Jev did not validate the known probe state".to_owned());
        }
        Ok::<_, String>(())
    };
    match tokio::time::timeout(timeout, result).await {
        Ok(Ok(())) => ok_row(
            SmokeRowInput::new("decision", started, "typed decision returned")
                .with_configuration(model, endpoint),
        ),
        Ok(Err(error)) => failed_row(
            SmokeRowInput::new("decision", started, error).with_configuration(model, endpoint),
        ),
        Err(_) => failed_row(
            SmokeRowInput::new("decision", started, "request timed out")
                .with_configuration(model, endpoint),
        ),
    }
}

async fn smoke_embedding(project: &Path, timeout: Duration) -> SmokeRow {
    let started = Instant::now();
    let configured = match load_exact_project_llm_tier(project, ProjectLlmTier::Embedding) {
        Ok(Some(config)) => config,
        Ok(None) => return missing_row("embedding", true, started),
        Err(error) => {
            return failed_row(SmokeRowInput::new("embedding", started, error.to_string()));
        }
    };
    let model = Some(configured.model().to_owned());
    let endpoint = Some(configured.endpoint().to_owned());
    let result = async {
        let settings = EmbeddingSettings::from_project_config(&configured)
            .map_err(|error| error.to_string())?;
        let client = OpenAiEmbeddingClient::new(settings).map_err(|error| error.to_string())?;
        let batch = client
            .embed(&["Cartograph LLM smoke embedding probe".to_owned()])
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(batch.dimension())
    };
    match tokio::time::timeout(timeout, result).await {
        Ok(Ok(dimension)) => ok_row(
            SmokeRowInput::new(
                "embedding",
                started,
                format!("one finite vector returned ({dimension} dimensions)"),
            )
            .with_configuration(model, endpoint),
        ),
        Ok(Err(error)) => failed_row(
            SmokeRowInput::new("embedding", started, error).with_configuration(model, endpoint),
        ),
        Err(_) => failed_row(
            SmokeRowInput::new("embedding", started, "request timed out")
                .with_configuration(model, endpoint),
        ),
    }
}

async fn smoke_chat(request: ChatSmokeRequest<'_>) -> SmokeRow {
    let ChatSmokeRequest {
        project,
        target:
            ChatSmokeTarget {
                tier,
                label,
                required,
            },
        timeout,
    } = request;
    let started = Instant::now();
    let configured = match load_exact_project_llm_tier(project, tier) {
        Ok(Some(config)) => config,
        Ok(None) => return missing_row(label, required, started),
        Err(error) => {
            return failed_row(SmokeRowInput::new(label, started, error.to_string()));
        }
    };
    let model = Some(configured.model().to_owned());
    let endpoint = Some(configured.endpoint().to_owned());
    let result = async {
        let settings =
            ChatSettings::from_project_config(&configured).map_err(|error| error.to_string())?;
        OpenAiChatClient::new(settings)
            .map_err(|error| error.to_string())?
            .complete_message(ChatMessageRequest::new(
                "Reply with a short acknowledgement.",
                "Cartograph LLM smoke test",
                Some(24),
            ))
            .await
            .map_err(|error| error.to_string())
    };
    match tokio::time::timeout(timeout, result).await {
        Ok(Ok(completion)) => ok_row(
            SmokeRowInput::new(
                label,
                started,
                format!(
                    "chat completion returned {} UTF-8 bytes",
                    completion.content().len()
                ),
            )
            .with_configuration(model, endpoint),
        ),
        Ok(Err(error)) => failed_row(
            SmokeRowInput::new(label, started, error).with_configuration(model, endpoint),
        ),
        Err(_) => failed_row(
            SmokeRowInput::new(label, started, "request timed out")
                .with_configuration(model, endpoint),
        ),
    }
}

async fn smoke_rerank(project: &Path, timeout: Duration) -> SmokeRow {
    let started = Instant::now();
    let configured = match load_exact_project_llm_tier(project, ProjectLlmTier::Reranker) {
        Ok(Some(config)) => config,
        Ok(None) => return missing_row("rerank", false, started),
        Err(error) => {
            return failed_row(SmokeRowInput::new("rerank", started, error.to_string()));
        }
    };
    let model = Some(configured.model().to_owned());
    let endpoint = Some(configured.endpoint().to_owned());
    let result = async {
        let settings = RerankSettings::try_from_project(project)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "rerankerLlm is not configured".to_owned())?;
        OpenAiRerankClient::new(settings)
            .map_err(|error| error.to_string())?
            .rerank(
                "code graph search",
                &[
                    "code graph symbol search".to_owned(),
                    "banana bread recipe".to_owned(),
                ],
            )
            .await
            .map_err(|error| error.to_string())
    };
    match tokio::time::timeout(timeout, result).await {
        Ok(Ok(batch)) => ok_row(
            SmokeRowInput::new(
                "rerank",
                started,
                format!("{} finite normalized scores returned", batch.scores().len()),
            )
            .with_configuration(model, endpoint),
        ),
        Ok(Err(error)) => failed_row(
            SmokeRowInput::new("rerank", started, error).with_configuration(model, endpoint),
        ),
        Err(_) => failed_row(
            SmokeRowInput::new("rerank", started, "request timed out")
                .with_configuration(model, endpoint),
        ),
    }
}

fn ok_row(input: SmokeRowInput) -> SmokeRow {
    input.into_row(SmokeStatus::Ok)
}

fn failed_row(input: SmokeRowInput) -> SmokeRow {
    input.into_row(SmokeStatus::Fail)
}

fn overall_smoke_status(rows: &[SmokeRow]) -> OverallSmokeStatus {
    if rows.iter().any(|row| row.status == SmokeStatus::Fail) {
        OverallSmokeStatus::Fail
    } else {
        OverallSmokeStatus::Ok
    }
}

fn missing_row(tier: &'static str, required: bool, started: Instant) -> SmokeRow {
    SmokeRow {
        tier,
        status: if required {
            SmokeStatus::Fail
        } else {
            SmokeStatus::Skip
        },
        model: None,
        endpoint: None,
        duration_ms: elapsed_ms(started),
        detail: missing_tier_detail(tier, required),
    }
}

fn missing_tier_detail(tier: &str, required: bool) -> String {
    if required {
        return format!("{tier} LLM tier is not configured");
    }
    match tier {
        "summarize" => "summarize tier is not configured; indexed source and deterministic retrieval remain active".to_owned(),
        "rerank" => {
            "rerank tier is not configured; semantic cosine ordering remains active".to_owned()
        }
        _ => format!(
            "{tier} tier is not configured; retrieval remains available without generated chat"
        ),
    }
}

fn render_smoke(report: &SmokeReport) {
    println!("## cartograph llm smoke\n");
    for row in &report.rows {
        let marker = match row.status {
            SmokeStatus::Ok => "✓",
            SmokeStatus::Skip => "○",
            SmokeStatus::Fail => "✗",
        };
        let location = match (&row.endpoint, &row.model) {
            (Some(endpoint), Some(model)) => format!(" — {endpoint} / {model}"),
            _ => String::new(),
        };
        println!(
            "{marker} **{}**{location} ({}ms)\n  {}",
            row.tier, row.duration_ms, row.detail
        );
    }
    println!(
        "\nOverall: {:?} ({}ms)",
        report.overall_status, report.duration_ms
    );
}

async fn run_install(arguments: InstallArguments) -> Result<ExitCode, String> {
    let project = prepare_install_project(&arguments)?;
    let database = prepare_install_database(&project, &arguments).await?;
    let model_state = prepare_install_models(&project, &arguments).await?;
    let doctor = crate::build_doctor_report_with_settings(crate::DoctorReportInput {
        project_path: project.clone(),
        fix: false,
        skip_project_checks: false,
        explicit_database_settings: database.doctor_settings.as_ref(),
    })
    .await?;
    let report = InstallReport {
        models: model_state.models,
        config_written: model_state.config_written,
        minimal: arguments.models.minimal,
        database: database.kind,
        database_migrations_applied: database.migrations_applied,
        doctor,
        next_steps: install_next_steps(&project, model_state.config_written),
    };
    render_install_report(&report, arguments.json)?;
    Ok(if report.doctor.ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

fn prepare_install_project(arguments: &InstallArguments) -> Result<PathBuf, String> {
    let project = arguments
        .path
        .canonicalize()
        .map_err(|_| "LLM install project path must be an existing directory".to_owned())?;
    if !project.is_dir() {
        return Err("LLM install project path must be an existing directory".to_owned());
    }
    let mut state_fixes = Vec::new();
    let mut state_checks = Vec::new();
    crate::check_or_fix_project_state(&mut crate::ProjectStateCheckInput {
        project_path: &project,
        fix: true,
        fixes: &mut state_fixes,
        checks: &mut state_checks,
    })?;
    if state_checks
        .iter()
        .any(|check| check.status == crate::DoctorStatus::Fail)
    {
        return Err("LLM install could not initialize private project state".to_owned());
    }
    Ok(project)
}

struct InstallDatabaseState {
    kind: &'static str,
    migrations_applied: usize,
    doctor_settings: Option<DatabaseSettings>,
}

async fn prepare_install_database(
    project: &Path,
    arguments: &InstallArguments,
) -> Result<InstallDatabaseState, String> {
    let explicit_database = resolve_install_database_settings(arguments)?;
    if let Some(settings) = explicit_database {
        let pool = cartograph_db::connect(&settings)
            .await
            .map_err(|error| error.to_string())?;
        let database = CartographDatabase::new(pool, settings.schema().clone());
        let migrations = database
            .migrate()
            .await
            .map_err(|error| error.to_string())?;
        database.close().await;
        return Ok(InstallDatabaseState {
            kind: "external",
            migrations_applied: migrations.applied_versions.len(),
            doctor_settings: Some(settings),
        });
    }
    let port = crate::resolve_managed_database_port(project, None).await?;
    let report = ManagedDatabase::new(project, port)
        .map_err(|error| error.to_string())?
        .lifecycle()
        .start()
        .await
        .map_err(|error| error.to_string())?;
    Ok(InstallDatabaseState {
        kind: "managed",
        migrations_applied: report.migrations.applied_versions.len(),
        doctor_settings: None,
    })
}

struct InstallModelState {
    models: Option<cartograph_llm::InstallModelsReport>,
    config_written: bool,
}

async fn prepare_install_models(
    project: &Path,
    arguments: &InstallArguments,
) -> Result<InstallModelState, String> {
    let directory = arguments.dir.clone().unwrap_or(default_models_directory()?);
    let models = if arguments.models.no_models {
        None
    } else {
        Some(
            install_recommended_models(
                InstallModelsOptions::new(
                    directory.clone(),
                    arguments.models.minimal,
                    arguments.concurrency,
                )
                .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?,
        )
    };
    let config_written = !arguments.models.no_models;
    if config_written {
        let (inputs, cleared) = local_inputs_in(&directory, arguments.models.minimal)?;
        write_project_llm_configuration(project, &inputs, &cleared)
            .map_err(|error| error.to_string())?;
    }
    Ok(InstallModelState {
        models,
        config_written,
    })
}

fn install_next_steps(project: &Path, config_written: bool) -> Vec<String> {
    if config_written {
        vec![
            format!("Run `cartograph backend start {}`.", project.display()),
            format!("Run `cartograph llm smoke {}`.", project.display()),
        ]
    } else {
        vec![format!(
            "Run `cartograph llm setup {}` to select the externally managed provider.",
            project.display()
        )]
    }
}

fn render_install_report(report: &InstallReport, json: bool) -> Result<(), String> {
    if json {
        print_json(&report)?;
    } else {
        println!(
            "LLM install complete: downloads={}, configWritten={}, database={}",
            report.models.is_some(),
            report.config_written,
            report.database,
        );
        for step in &report.next_steps {
            println!("- {step}");
        }
        println!();
        print!("{}", crate::render_doctor_report(&report.doctor));
    }
    Ok(())
}

fn resolve_install_database_settings(
    arguments: &InstallArguments,
) -> Result<Option<DatabaseSettings>, String> {
    validate_install_database_modes(arguments)?;
    let connection_override = install_has_connection_override(arguments);
    let Some(settings) = base_install_database_settings(arguments, connection_override)? else {
        return Ok(None);
    };
    apply_install_database_overrides(settings, arguments).map(Some)
}

fn validate_install_database_modes(arguments: &InstallArguments) -> Result<(), String> {
    match arguments.database_provider.as_deref() {
        None | Some("postgres" | "postgresql") => {}
        Some("sqlite") => {
            return Err(
                "SQLite was intentionally removed in Cartograph v2; use PostgreSQL 18 with ParadeDB and pgvector"
                    .to_owned(),
            );
        }
        Some(_) => return Err("database provider must be postgres or postgresql".to_owned()),
    }
    match arguments.database_pgvector.as_deref() {
        None | Some("auto" | "require") => {}
        Some("off") => {
            return Err("pgvector cannot be disabled in Cartograph v2".to_owned());
        }
        Some(_) => return Err("database pgvector mode must be auto or require".to_owned()),
    }
    Ok(())
}

fn install_has_connection_override(arguments: &InstallArguments) -> bool {
    arguments.database_url.is_some()
        || arguments.database_schema.is_some()
        || arguments.database_max_connections.is_some()
        || arguments.database_query_timeout_ms.is_some()
        || arguments.database_connection_timeout_seconds.is_some()
        || arguments.database_ssl
}

fn base_install_database_settings(
    arguments: &InstallArguments,
    connection_override: bool,
) -> Result<Option<DatabaseSettings>, String> {
    if let Some(url) = arguments.database_url.as_deref() {
        DatabaseSettings::parse(url, None, None)
            .map(Some)
            .map_err(|error| error.to_string())
    } else if env::var_os(DATABASE_URL_ENV).is_some() {
        DatabaseSettings::from_env()
            .map(Some)
            .map_err(|error| error.to_string())
    } else if connection_override {
        Err(
            "--database-url or CARTOGRAPH_DATABASE_URL is required with external PostgreSQL connection overrides"
                .to_owned(),
        )
    } else {
        Ok(None)
    }
}

fn apply_install_database_overrides(
    mut settings: DatabaseSettings,
    arguments: &InstallArguments,
) -> Result<DatabaseSettings, String> {
    if let Some(max_connections) = arguments.database_max_connections {
        settings = settings
            .with_max_connections(max_connections)
            .map_err(|error| error.to_string())?;
    }
    if let Some(seconds) = arguments.database_connection_timeout_seconds {
        settings = settings
            .with_acquire_timeout_ms(seconds.saturating_mul(1_000))
            .map_err(|error| error.to_string())?;
    }
    if let Some(query_timeout_ms) = arguments.database_query_timeout_ms {
        let raw = query_timeout_ms.to_string();
        settings = settings
            .with_query_timeout_ms(Some(&raw))
            .map_err(|error| error.to_string())?;
    }
    if let Some(schema) = arguments.database_schema.as_deref() {
        settings = settings
            .with_schema(schema)
            .map_err(|error| error.to_string())?;
    }
    if arguments.database_ssl {
        settings = settings.with_require_ssl(true);
    }
    Ok(settings)
}

fn local_inputs_in(
    directory: &Path,
    minimal: bool,
) -> Result<(Vec<ProjectLlmTierInput>, Vec<ProjectLlmTier>), String> {
    let model = |filename: &str| directory.join(filename).to_string_lossy().into_owned();
    let mut inputs = vec![
        TierInputRequest::new(
            ProjectLlmTier::Embedding,
            LLAMA_EMBED_ENDPOINT,
            model("jina-embeddings-v2-base-code.Q4_K_M.gguf"),
        )
        .with_concurrency(4)
        .build()?,
        TierInputRequest::new(
            ProjectLlmTier::Summarize,
            LLAMA_CHAT_ENDPOINT,
            model("qwen2.5-coder-3b-instruct-q4_k_m.gguf"),
        )
        .build()?,
        TierInputRequest::new(
            ProjectLlmTier::Local,
            LLAMA_CHAT_ENDPOINT,
            model("qwen2.5-coder-3b-instruct-q4_k_m.gguf"),
        )
        .build()?,
        TierInputRequest::new(
            ProjectLlmTier::Classify,
            LLAMA_CHAT_ENDPOINT,
            model("qwen2.5-coder-3b-instruct-q4_k_m.gguf"),
        )
        .build()?,
    ];
    let cleared = if minimal {
        vec![ProjectLlmTier::Ask, ProjectLlmTier::Reranker]
    } else {
        inputs.push(
            TierInputRequest::new(
                ProjectLlmTier::Ask,
                LLAMA_ASK_ENDPOINT,
                model("qwen2.5-coder-7b-instruct-q4_k_m.gguf"),
            )
            .with_concurrency(1)
            .build()?,
        );
        inputs.push(
            TierInputRequest::new(
                ProjectLlmTier::Reranker,
                LLAMA_RERANK_ENDPOINT,
                model("bge-reranker-v2-m3-Q4_K_M.gguf"),
            )
            .build()?,
        );
        Vec::new()
    };
    Ok((inputs, cleared))
}

fn default_models_directory() -> Result<PathBuf, String> {
    let root = match env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        Some(home) => PathBuf::from(home),
        None => env::current_dir()
            .map_err(|_| "could not resolve the Cartograph models directory".to_owned())?,
    };
    Ok(root.join(".cartograph").join("models"))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn print_json(value: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|_| "could not serialize LLM report".to_owned())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_arguments(path: &Path) -> SetupArguments {
        SetupArguments {
            path: path.to_path_buf(),
            preset: None,
            tier: None,
            endpoint: None,
            model: None,
            command: None,
            command_args: Vec::new(),
            input: None,
            prompt_template: None,
            response_format: None,
            response_path: None,
            credentials: SetupCredentialArguments {
                api_key_env: None,
                api_key_command: None,
                api_key_args: Vec::new(),
                clear_credentials: false,
            },
            jev_features: None,
            minimal: false,
            yes: false,
            json: false,
        }
    }

    #[test]
    fn jev_setup_changes_only_the_decision_tier_and_rejects_provider_overrides() {
        let mut arguments = setup_arguments(Path::new("."));
        let (inputs, cleared) =
            setup_inputs(&arguments, SetupPreset::Jev).unwrap_or_else(|e| panic!("Jev setup: {e}"));
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].tier(), ProjectLlmTier::Decision);
        assert_eq!(cleared, []);
        arguments.credentials.clear_credentials = true;
        let (inputs, cleared) = setup_inputs(&arguments, SetupPreset::Jev)
            .unwrap_or_else(|e| panic!("Jev disable: {e}"));
        assert!(inputs.is_empty());
        assert_eq!(cleared, [ProjectLlmTier::Decision]);
        arguments.credentials.api_key_env = Some("MY_JEV_KEY".to_owned());
        assert!(setup_inputs(&arguments, SetupPreset::Jev).is_err());
        arguments.credentials.clear_credentials = false;
        arguments.endpoint = Some("https://untrusted.example".to_owned());
        assert!(setup_inputs(&arguments, SetupPreset::Jev).is_err());
    }

    #[test]
    fn jev_features_are_written_deduplicated_and_rejected_for_other_presets() {
        let root = tempfile::tempdir().unwrap_or_else(|e| panic!("fixture: {e}"));
        let mut arguments = setup_arguments(root.path());
        arguments.jev_features = Some(vec![
            JevFeatureArgument::Context,
            JevFeatureArgument::Explore,
            JevFeatureArgument::Context,
        ]);
        let (inputs, _) = setup_inputs(&arguments, SetupPreset::Jev)
            .unwrap_or_else(|e| panic!("Jev features: {e}"));
        cartograph_llm::write_project_llm_tiers(root.path(), &inputs)
            .unwrap_or_else(|e| panic!("write: {e}"));
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join(".cartograph/config.json"))
                .unwrap_or_else(|e| panic!("read: {e}")),
        )
        .unwrap_or_else(|e| panic!("json: {e}"));
        assert_eq!(
            written["llm"]["decisionLlm"]["features"],
            serde_json::json!(["context", "explore"])
        );
        assert!(setup_inputs(&arguments, SetupPreset::Ollama).is_err());
        arguments.credentials.clear_credentials = true;
        assert!(setup_inputs(&arguments, SetupPreset::Jev).is_err());
    }

    fn install_arguments() -> InstallArguments {
        InstallArguments {
            path: PathBuf::from("."),
            models: ModelInstallFlags {
                minimal: false,
                no_models: true,
            },
            dir: None,
            concurrency: 2,
            database_provider: None,
            database_url: None,
            database_schema: None,
            database_pgvector: None,
            database_max_connections: None,
            database_query_timeout_ms: None,
            database_connection_timeout_seconds: None,
            database_ssl: false,
            json: false,
        }
    }

    #[test]
    fn credential_environment_overrides_are_tier_scoped_and_bounded_by_the_library() {
        let parsed = parse_tier_environment_override("summarize=CARTOGRAPH_CHAT_KEY")
            .unwrap_or_else(|error| panic!("tier environment parse failed: {error}"));
        assert_eq!(parsed.tier, ProjectLlmTier::Summarize);
        assert_eq!(parsed.environment, "CARTOGRAPH_CHAT_KEY");
        assert!(parse_tier_environment_override("unknown=KEY").is_err());
        assert!(parse_tier_environment_override("summarize").is_err());
        assert!(parse_tier_environment_override("summarize=").is_err());
    }

    #[test]
    fn presets_preserve_split_chat_and_minimal_fallback_contracts() {
        let (full, cleared) =
            ollama_inputs(false).unwrap_or_else(|error| panic!("ollama preset failed: {error}"));
        assert!(
            full.iter()
                .any(|input| input.tier() == ProjectLlmTier::Local)
        );
        assert!(full.iter().any(|input| input.tier() == ProjectLlmTier::Ask));
        assert_eq!(cleared, vec![ProjectLlmTier::Reranker]);

        let (minimal, cleared) = ollama_inputs(true)
            .unwrap_or_else(|error| panic!("minimal ollama preset failed: {error}"));
        assert!(
            !minimal
                .iter()
                .any(|input| input.tier() == ProjectLlmTier::Ask)
        );
        assert!(cleared.contains(&ProjectLlmTier::Ask));
        assert!(cleared.contains(&ProjectLlmTier::Reranker));
    }

    #[test]
    fn hybrid_presets_write_native_provider_tiers_without_sqlite_or_inline_secrets() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::create_dir(root.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("state directory failed: {error}"));
        let arguments = SetupArguments {
            path: root.path().to_path_buf(),
            preset: None,
            tier: None,
            endpoint: None,
            model: None,
            command: None,
            command_args: Vec::new(),
            input: None,
            prompt_template: None,
            response_format: None,
            response_path: None,
            credentials: SetupCredentialArguments {
                api_key_env: None,
                api_key_command: None,
                api_key_args: Vec::new(),
                clear_credentials: false,
            },
            jev_features: None,
            minimal: false,
            yes: false,
            json: false,
        };
        let (bridge, bridge_cleared) = hybrid_claude_inputs(&arguments)
            .unwrap_or_else(|error| panic!("bridge preset failed: {error}"));
        assert_eq!(bridge.len(), 5);
        assert_eq!(bridge_cleared, vec![ProjectLlmTier::Reranker]);
        write_project_llm_configuration(root.path(), &bridge, &bridge_cleared)
            .unwrap_or_else(|error| panic!("bridge config failed: {error}"));
        let bridge_config = std::fs::read_to_string(root.path().join(".cartograph/config.json"))
            .unwrap_or_else(|error| panic!("bridge config read failed: {error}"));
        assert!(bridge_config.contains("\"provider\": \"cli-bridge\""));
        assert!(bridge_config.contains("\"command\": \"claude\""));
        assert!(bridge_config.contains("\"responseFormat\": \"claude\""));
        assert!(!bridge_config.contains("apiKey"));
        assert!(!bridge_config.contains("sqlite"));

        let (anthropic, anthropic_cleared) = hybrid_anthropic_inputs(&arguments)
            .unwrap_or_else(|error| panic!("anthropic preset failed: {error}"));
        assert_eq!(anthropic.len(), 5);
        write_project_llm_configuration(root.path(), &anthropic, &anthropic_cleared)
            .unwrap_or_else(|error| panic!("anthropic config failed: {error}"));
        let anthropic_config = std::fs::read_to_string(root.path().join(".cartograph/config.json"))
            .unwrap_or_else(|error| panic!("anthropic config read failed: {error}"));
        assert!(anthropic_config.contains("\"provider\": \"anthropic-api\""));
        assert!(anthropic_config.contains("\"apiKeyEnv\": \"ANTHROPIC_API_KEY\""));
        assert!(!anthropic_config.contains("apiKey\""));
    }

    #[test]
    fn explicit_postgres_install_options_are_validated_without_persisting_the_url() {
        let mut arguments = install_arguments();
        arguments.database_provider = Some("postgresql".to_owned());
        arguments.database_url =
            Some("postgresql://cartograph@127.0.0.1:55432/cartograph".to_owned());
        arguments.database_schema = Some("Review_Project".to_owned());
        arguments.database_pgvector = Some("require".to_owned());
        arguments.database_max_connections = Some(12);
        arguments.database_query_timeout_ms = Some(45_000);
        arguments.database_connection_timeout_seconds = Some(7);
        arguments.database_ssl = true;

        let settings = resolve_install_database_settings(&arguments)
            .unwrap_or_else(|error| panic!("explicit database settings failed: {error}"))
            .unwrap_or_else(|| panic!("explicit database settings disappeared"));
        assert_eq!(settings.schema().as_str(), "review_project");
        assert_eq!(settings.max_connections().get(), 12);
        assert_eq!(settings.acquire_timeout(), Duration::from_secs(7));
        assert_eq!(settings.query_timeout(), Duration::from_secs(45));
        assert!(settings.require_ssl());

        arguments.database_provider = Some("sqlite".to_owned());
        let error = resolve_install_database_settings(&arguments)
            .err()
            .unwrap_or_else(|| panic!("SQLite install mode was accepted"));
        assert!(error.contains("intentionally removed"));
        assert!(!error.contains("private"));

        arguments.database_provider = Some("postgres".to_owned());
        arguments.database_pgvector = Some("off".to_owned());
        assert!(resolve_install_database_settings(&arguments).is_err());
    }

    #[test]
    fn every_setup_preset_has_explicit_inputs_clears_and_custom_field_policy() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let mut arguments = setup_arguments(root.path());

        let (local, local_cleared) = local_inputs_in(root.path(), false)
            .unwrap_or_else(|error| panic!("local preset failed: {error}"));
        assert_eq!(local.len(), 6);
        assert_eq!(local_cleared, []);
        let (minimal, minimal_cleared) = local_inputs_in(root.path(), true)
            .unwrap_or_else(|error| panic!("minimal local preset failed: {error}"));
        assert_eq!(minimal.len(), 4);
        assert_eq!(
            minimal_cleared,
            vec![ProjectLlmTier::Ask, ProjectLlmTier::Reranker]
        );

        arguments.tier = Some(LlmTierArgument::Chat);
        arguments.endpoint = Some(OPENAI_V1_ENDPOINT.to_owned());
        arguments.model = Some("gpt-fixture".to_owned());
        arguments.credentials.api_key_env = Some("OPENAI_FIXTURE_KEY".to_owned());
        let (custom, cleared) = setup_inputs(&arguments, SetupPreset::Custom)
            .unwrap_or_else(|error| panic!("custom preset failed: {error}"));
        assert_eq!(custom.len(), 1);
        assert_eq!(cleared, []);
        assert_eq!(custom[0].tier(), ProjectLlmTier::Summarize);
        assert!(setup_inputs(&arguments, SetupPreset::LocalLlamaCpp).is_err());
        assert!(setup_inputs(&arguments, SetupPreset::Ollama).is_err());
        assert!(setup_inputs(&arguments, SetupPreset::Skip).is_err());

        arguments.tier = None;
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
        arguments.tier = Some(LlmTierArgument::Embed);
        arguments.endpoint = None;
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
        arguments.endpoint = Some(OPENAI_ENDPOINT.to_owned());
        arguments.model = None;
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
        arguments.model = Some("text-embedding-3-small".to_owned());
        arguments.credentials.api_key_env = None;
        arguments.credentials.clear_credentials = true;
        let (credential_free, _) = setup_inputs(&arguments, SetupPreset::Custom)
            .unwrap_or_else(|error| panic!("credential clear input failed: {error}"));
        assert_eq!(credential_free.len(), 1);
        arguments.credentials.api_key_env = Some("OPENAI_FIXTURE_KEY".to_owned());
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
        arguments.credentials.api_key_env = None;
        arguments.credentials.clear_credentials = false;
        let (cloud, _) = setup_inputs(&arguments, SetupPreset::CloudOpenAi)
            .unwrap_or_else(|error| panic!("cloud preset failed: {error}"));
        assert_eq!(cloud[0].tier(), ProjectLlmTier::Embedding);

        arguments.minimal = true;
        assert!(setup_inputs(&arguments, SetupPreset::CloudOpenAi).is_err());
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
        arguments.minimal = false;
        arguments.tier = None;
        arguments.endpoint = None;
        arguments.model = None;
        let (skipped, skipped_clears) = setup_inputs(&arguments, SetupPreset::Skip)
            .unwrap_or_else(|error| panic!("skip preset failed: {error}"));
        assert!(skipped.is_empty());
        assert_eq!(skipped_clears, []);

        for (argument, tier) in [
            (LlmTierArgument::Local, ProjectLlmTier::Local),
            (LlmTierArgument::Ask, ProjectLlmTier::Ask),
            (LlmTierArgument::Classify, ProjectLlmTier::Classify),
            (LlmTierArgument::Reranker, ProjectLlmTier::Reranker),
        ] {
            assert_eq!(ProjectLlmTier::from(argument), tier);
        }
    }

    #[test]
    fn detection_recommendation_and_smoke_rows_keep_required_failures_explicit() {
        let detected = vec![DetectedEndpoint {
            endpoint: OLLAMA_ENDPOINT.to_owned(),
            reachable: true,
            openai_compatible: true,
            models: vec!["fixture".to_owned()],
        }];
        assert_eq!(recommend_preset(&detected), SetupPreset::Ollama);
        assert_eq!(recommend_preset(&[]), SetupPreset::LocalLlamaCpp);
        render_detection(&detected, SetupPreset::Ollama);

        let required = missing_row("embedding", true, Instant::now());
        assert_eq!(required.status, SmokeStatus::Fail);
        assert!(required.detail.contains("not configured"));
        let optional = missing_row("ask", false, Instant::now());
        assert_eq!(optional.status, SmokeStatus::Skip);
        assert!(optional.detail.contains("without generated chat"));
        let optional_summarize = missing_row("summarize", false, Instant::now());
        assert_eq!(optional_summarize.status, SmokeStatus::Skip);
        assert!(
            optional_summarize
                .detail
                .contains("deterministic retrieval")
        );
        let ok = ok_row(
            SmokeRowInput::new("summarize", Instant::now(), "bounded response").with_configuration(
                Some("fixture".to_owned()),
                Some(LLAMA_CHAT_ENDPOINT.to_owned()),
            ),
        );
        let failed = failed_row(SmokeRowInput::new(
            "rerank",
            Instant::now(),
            "bounded failure",
        ));
        let report = SmokeReport {
            overall_status: OverallSmokeStatus::Fail,
            rows: vec![required, optional, optional_summarize, ok, failed],
            duration_ms: 1,
        };
        render_smoke(&report);
        assert!(elapsed_ms(Instant::now()) <= 1_000);
    }

    #[test]
    fn cli_bridge_preset_requires_and_persists_the_shell_free_transport_contract() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let mut arguments = setup_arguments(root.path());
        arguments.tier = Some(LlmTierArgument::Chat);
        arguments.model = Some("agent-model".to_owned());
        arguments.command = Some("agent-cli".to_owned());
        arguments.command_args = vec!["-p".to_owned(), "{prompt}".to_owned()];
        arguments.input = Some(CliBridgeInputArgument::Arg);
        arguments.response_format = Some(CliBridgeResponseArgument::Raw);

        let (inputs, cleared) = setup_inputs(&arguments, SetupPreset::CliBridge)
            .unwrap_or_else(|error| panic!("CLI bridge preset failed: {error}"));
        assert_eq!(inputs.len(), 1);
        assert_eq!(cleared, []);
        write_project_llm_configuration(root.path(), &inputs, &cleared)
            .unwrap_or_else(|error| panic!("CLI bridge write failed: {error}"));
        let config = std::fs::read_to_string(root.path().join(".cartograph/config.json"))
            .unwrap_or_else(|error| panic!("CLI bridge config read failed: {error}"));
        assert!(config.contains("\"provider\": \"cli-bridge\""));
        assert!(config.contains("\"command\": \"agent-cli\""));
        assert!(config.contains("\"responseFormat\": \"raw\""));
        assert!(!config.contains("apiKey"));

        arguments.command_args = vec!["{unknown}".to_owned()];
        assert!(setup_inputs(&arguments, SetupPreset::CliBridge).is_err());
        arguments.command_args = vec!["{prompt}".to_owned()];
        arguments.tier = Some(LlmTierArgument::Embed);
        assert!(setup_inputs(&arguments, SetupPreset::CliBridge).is_err());
    }

    #[test]
    fn optional_unconfigured_smoke_tiers_do_not_degrade_passing_configured_tiers() {
        let rows = vec![
            ok_row(SmokeRowInput::new(
                "embedding",
                Instant::now(),
                "bounded response",
            )),
            missing_row("summarize", false, Instant::now()),
            missing_row("ask", false, Instant::now()),
            missing_row("local", false, Instant::now()),
            missing_row("classify", false, Instant::now()),
            ok_row(SmokeRowInput::new(
                "rerank",
                Instant::now(),
                "bounded response",
            )),
        ];
        assert_eq!(overall_smoke_status(&rows), OverallSmokeStatus::Ok);
    }

    #[tokio::test]
    async fn missing_jev_key_preserves_configuration_in_smoke_diagnostics() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let variable = format!("CARTOGRAPH_SMOKE_MISSING_JEV_{}", std::process::id());
        assert!(std::env::var_os(&variable).is_none());
        cartograph_llm::write_project_llm_tiers(
            root.path(),
            &[ProjectLlmTierInput::jev(&variable).unwrap_or_else(|error| panic!("tier: {error}"))],
        )
        .unwrap_or_else(|error| panic!("config: {error}"));
        let row = smoke_jev(root.path(), Duration::from_secs(1)).await;
        assert_eq!(row.status, SmokeStatus::Fail);
        assert_eq!(row.model.as_deref(), Some(cartograph_llm::JEV_MODEL));
        assert_eq!(row.endpoint.as_deref(), Some(cartograph_llm::JEV_ENDPOINT));
        assert!(row.detail.contains(&variable));
        assert!(row.detail.contains("not set in this process"));
    }

    #[tokio::test]
    async fn skipped_setup_and_unconfigured_smoke_are_noninteractive_and_deterministic() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let mut setup = setup_arguments(root.path());
        setup.preset = Some(SetupPreset::Skip);
        setup.json = true;
        assert_eq!(
            run(LlmCommand::Setup(Box::new(setup))).await,
            Ok(ExitCode::SUCCESS)
        );
        assert!(!root.path().join(".cartograph/config.json").exists());

        assert_eq!(
            run(LlmCommand::Smoke(SmokeArguments {
                path: root.path().to_path_buf(),
                timeout_ms: 50,
                json: false,
            }))
            .await,
            Ok(ExitCode::FAILURE)
        );
        let unreachable = detect_one(UNREACHABLE_LOOPBACK_ENDPOINT).await;
        assert!(!unreachable.reachable);
        assert!(!unreachable.openai_compatible);
        assert_eq!(unreachable.models, [] as [String; 0]);
    }

    #[test]
    fn credential_commands_are_written_for_remote_presets_and_exclusive_with_other_sources() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let mut arguments = setup_arguments(root.path());
        arguments.credentials.api_key_command = Some("/opt/bin/secret-helper".to_owned());
        arguments.credentials.api_key_args = vec!["get".to_owned(), "--field=key".to_owned()];
        let (jev, _) = setup_inputs(&arguments, SetupPreset::Jev)
            .unwrap_or_else(|error| panic!("Jev command setup failed: {error}"));
        arguments.tier = Some(LlmTierArgument::Chat);
        arguments.endpoint = Some(OPENAI_V1_ENDPOINT.to_owned());
        arguments.model = Some("gpt-fixture".to_owned());
        let (custom, _) = setup_inputs(&arguments, SetupPreset::Custom)
            .unwrap_or_else(|error| panic!("custom command setup failed: {error}"));
        write_project_llm_configuration(root.path(), &[jev, custom].concat(), &[])
            .unwrap_or_else(|error| panic!("config write failed: {error}"));
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join(".cartograph/config.json"))
                .unwrap_or_else(|error| panic!("config read failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("config JSON failed: {error}"));
        let argv = serde_json::json!(["/opt/bin/secret-helper", "get", "--field=key"]);
        for tier in ["decisionLlm", "summarizeLlm"] {
            assert_eq!(written["llm"][tier]["apiKeyCommand"], argv, "{tier}");
            assert!(written["llm"][tier].get("apiKeyEnv").is_none(), "{tier}");
        }

        for preset in [
            SetupPreset::CliBridge,
            SetupPreset::HybridClaudeBridge,
            SetupPreset::Ollama,
        ] {
            assert!(setup_inputs(&arguments, preset).is_err(), "{preset:?}");
        }
        arguments.credentials.api_key_env = Some("OPENAI_FIXTURE_KEY".to_owned());
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
        arguments.credentials.api_key_env = None;
        arguments.credentials.clear_credentials = true;
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
        assert!(setup_inputs(&arguments, SetupPreset::Jev).is_err());
        arguments.credentials.clear_credentials = false;
        arguments.credentials.api_key_command = None;
        assert!(setup_inputs(&arguments, SetupPreset::Custom).is_err());
    }

    #[test]
    fn secret_bearing_arguments_never_print_in_debug_output() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            install: InstallArguments,
            #[command(flatten)]
            credentials: SetupCredentialArguments,
        }
        let parsed = <Harness as clap::Parser>::try_parse_from([
            "install",
            "--database-url",
            "postgresql://cartograph:debug-secret@127.0.0.1:1/cartograph",
            "--api-key-command",
            "/opt/helper",
            "--api-key-arg",
            "--token=helper-secret",
        ])
        .unwrap_or_else(|error| panic!("secret flags failed: {error}"));
        let rendered = format!("{:?} {:?}", parsed.install, parsed.credentials);
        assert!(!rendered.contains("debug-secret"), "{rendered}");
        assert!(!rendered.contains("helper-secret"), "{rendered}");
        assert!(rendered.contains("api_key_argument_count: 1"), "{rendered}");
        assert_eq!(
            parsed.install.database_url.as_deref(),
            Some("postgresql://cartograph:debug-secret@127.0.0.1:1/cartograph")
        );
    }

    #[test]
    fn command_line_credential_flags_are_mutually_exclusive() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            arguments: SetupArguments,
        }
        let parse = |flags: &[&str]| {
            <Harness as clap::Parser>::try_parse_from(
                ["setup", ".", "--preset", "jev"].iter().chain(flags),
            )
        };
        let parsed = parse(&[
            "--api-key-command",
            "/opt/helper",
            "--api-key-arg",
            "--item",
        ])
        .unwrap_or_else(|error| panic!("command flags failed: {error}"));
        assert_eq!(
            parsed.arguments.credentials.api_key_command.as_deref(),
            Some("/opt/helper")
        );
        assert_eq!(parsed.arguments.credentials.api_key_args, ["--item"]);
        for rejected in [
            &[
                "--api-key-env",
                "MY_KEY",
                "--api-key-command",
                "/opt/helper",
            ][..],
            &["--clear-credentials", "--api-key-command", "/opt/helper"],
            &["--api-key-arg", "orphan"],
        ] {
            assert!(parse(rejected).is_err(), "{rejected:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn smoke_reports_a_failing_credential_command_without_its_output() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let helper = root.path().join("smoke-helper");
        std::fs::write(
            &helper,
            "#!/bin/sh\nprintf 'smoke-token-value'\nprintf 'stderr-token-value' >&2\nexit 9\n",
        )
        .unwrap_or_else(|error| panic!("helper: {error}"));
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("chmod: {error}"));
        let command = CredentialCommand::new(vec![
            helper
                .to_str()
                .unwrap_or_else(|| panic!("helper path is not UTF-8"))
                .to_owned(),
        ])
        .unwrap_or_else(|error| panic!("command: {error}"));
        let input = ProjectLlmTierInput::jev("TYPESAFE_API_KEY")
            .and_then(|input| input.with_api_key_command(command))
            .unwrap_or_else(|error| panic!("tier: {error}"));
        cartograph_llm::write_project_llm_tiers(root.path(), &[input])
            .unwrap_or_else(|error| panic!("config: {error}"));
        let row = smoke_jev(root.path(), Duration::from_secs(5)).await;
        assert_eq!(row.status, SmokeStatus::Fail);
        assert_eq!(
            row.detail,
            "Cartograph Jev credential command `smoke-helper` exited with status 9"
        );
        assert_eq!(row.model.as_deref(), Some(cartograph_llm::JEV_MODEL));
    }
}
