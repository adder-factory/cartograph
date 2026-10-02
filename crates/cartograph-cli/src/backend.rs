use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cartograph_llm::{
    ProjectLlmTier, ProjectLlmTierConfig, load_exact_project_llm_tier,
    probe_openai_compatible_endpoint,
};
use clap::{Args, Subcommand, ValueEnum};
use futures_util::{StreamExt as _, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use url::Url;

const STATE_DIRECTORY: &str = "backends";
const PUBLIC_STATE_DIRECTORY: &str = ".cartograph/backends";
const PID_SCHEMA_VERSION: u8 = 2;
const MAXIMUM_STATE_FILE_BYTES: u64 = 1024 * 1024;
const MAXIMUM_STATE_ENTRIES: usize = 256;
const MAXIMUM_LOG_TAIL_BYTES: u64 = 512 * 1024;
const MAXIMUM_BACKEND_LOG_BYTES: u64 = 32 * 1024 * 1024;
const ROTATED_LOG_SUFFIX: &str = ".1";
const DEFAULT_LOG_LINES: u16 = 80;
const DEFAULT_CLEANUP_MINIMUM_AGE_HOURS: u16 = 24;
const DEFAULT_CLEANUP_MAXIMUM_DELETIONS: u16 = 64;
const MAXIMUM_CLEANUP_DELETIONS: u16 = 256;
const CLEANUP_CONFIRMATION: &str = "cleanup-backend-junk";
const STOP_GRACE: Duration = Duration::from_secs(3);
const STOP_POLL: Duration = Duration::from_millis(100);
const START_CONFIRMATION: Duration = Duration::from_millis(250);
const ENDPOINT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(any(unix, windows))]
const MAXIMUM_PROCESS_OUTPUT_BYTES: usize = 128 * 1024;
const MAXIMUM_PID_COMMAND_BYTES: usize = 4_096;
const MAXIMUM_PID_ARGUMENTS: usize = 256;
const MAXIMUM_PID_LABELS: usize = 16;
const MAXIMUM_PID_LABEL_BYTES: usize = 64;

#[derive(Debug, Subcommand)]
pub(super) enum BackendCommand {
    /// Show configured local llama-server backend status.
    Status(StatusArguments),
    /// Start eligible configured local llama-server processes in the background.
    Start(StartArguments),
    /// Restart managed processes so model/concurrency/argument changes take effect.
    Restart(RestartArguments),
    /// Stop only processes recorded and identity-checked by Cartograph.
    Stop(StopArguments),
    /// Tail bounded local logs for configured or orphaned managed processes.
    Logs(LogsArguments),
    /// Inspect or remove bounded stale backend state and rotated logs.
    Cleanup(CleanupArguments),
}

#[derive(Debug, Args)]
pub(super) struct StatusArguments {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(super) struct StartArguments {
    #[arg(default_value = ".")]
    path: PathBuf,
    /// llama-server executable path.
    #[arg(long = "bin", default_value = "llama-server")]
    binary: String,
    /// Report eligible processes without spawning them.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(super) struct RestartArguments {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long = "bin", default_value = "llama-server")]
    binary: String,
    /// Restrict the operation to one stable tier label.
    #[arg(long, value_enum)]
    tier: Option<BackendTier>,
    /// Force-kill a verified managed process after the graceful deadline.
    #[arg(long)]
    force: bool,
    /// Report eligible processes without changing them.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(super) struct StopArguments {
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Force-kill a verified managed process after the graceful deadline.
    #[arg(long)]
    force: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(super) struct LogsArguments {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long, value_enum)]
    tier: Option<BackendTier>,
    #[arg(long, default_value_t = DEFAULT_LOG_LINES, value_parser = clap::value_parser!(u16).range(1..=10_000))]
    lines: u16,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(super) struct CleanupArguments {
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Remove eligible junk. Without this flag the command is a dry run.
    #[arg(long)]
    apply: bool,
    /// Exact phrase required with --apply: cleanup-backend-junk.
    #[arg(long, requires = "apply")]
    confirm: Option<String>,
    #[arg(long, default_value_t = DEFAULT_CLEANUP_MINIMUM_AGE_HOURS)]
    minimum_age_hours: u16,
    #[arg(long, default_value_t = DEFAULT_CLEANUP_MAXIMUM_DELETIONS, value_parser = clap::value_parser!(u16).range(1..=MAXIMUM_CLEANUP_DELETIONS as i64))]
    maximum_deletions: u16,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum BackendTier {
    Embed,
    Summarize,
    Local,
    Ask,
    Classify,
    Rerank,
}

impl BackendTier {
    const fn label(self) -> &'static str {
        match self {
            Self::Embed => "embed",
            Self::Summarize => "summarize",
            Self::Local => "local",
            Self::Ask => "ask",
            Self::Classify => "classify",
            Self::Rerank => "rerank",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendSpec {
    id: String,
    labels: Vec<String>,
    endpoint: String,
    model_path: PathBuf,
    host: String,
    port: u16,
    parallel: u16,
    command: String,
    args: Vec<String>,
    externally_managed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BackendOrigin {
    Config,
    Orphan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum BackendState {
    Running,
    Starting,
    External,
    Stopped,
    MissingModel,
    InvalidState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackendPidRecord {
    schema_version: u8,
    pid: u32,
    started_at_unix_ms: u64,
    command: String,
    args: Vec<String>,
    endpoint: String,
    model_path: PathBuf,
    labels: Vec<String>,
    log_path: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackendPidVersion {
    schema_version: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendRow {
    spec: BackendSpec,
    origin: BackendOrigin,
    pid_file_path: PathBuf,
    log_path: PathBuf,
    pid_record: Option<BackendPidRecord>,
    #[serde(flatten)]
    process_health: BackendProcessHealth,
    #[serde(flatten)]
    artifact_health: BackendArtifactHealth,
    state: BackendState,
    state_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendProcessHealth {
    pid_alive: bool,
    endpoint_reachable: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendArtifactHealth {
    model_exists: bool,
    config_drift: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendStatusReport {
    project_path: PathBuf,
    state_directory: PathBuf,
    rows: Vec<BackendRow>,
    warnings: Vec<String>,
    unmanaged_reason: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionDisposition {
    labels: Vec<String>,
    endpoint: String,
    reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendActionReport {
    status: BackendStatusReport,
    changed: Vec<ActionDisposition>,
    skipped: Vec<ActionDisposition>,
    dry_run: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendLogEntry {
    labels: Vec<String>,
    path: PathBuf,
    exists: bool,
    content: String,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendLogsReport {
    status: BackendStatusReport,
    logs: Vec<BackendLogEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BackendCleanupKind {
    RotatedLog,
    InvalidPidState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BackendCleanupReason {
    BoundedRotatedLog,
    StaleInvalidPidState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct BackendCleanupSkipReason(&'static str);

impl BackendCleanupSkipReason {
    const ENTRY_UNREADABLE: Self = Self("entry_unreadable");
    const NON_UNICODE_ENTRY: Self = Self("non_unicode_entry");
    const INVALID_ENTRY_NAME: Self = Self("invalid_entry_name");
    const UNSAFE_ENTRY_TYPE: Self = Self("unsafe_entry_type");
    const MODIFICATION_TIME_UNAVAILABLE: Self = Self("modification_time_unavailable");
    const UNSUPPORTED_STATE_VERSION: Self = Self("unsupported_state_version");
    const CANDIDATE_CHANGED: Self = Self("candidate_changed");
    const REMOVAL_FAILED: Self = Self("removal_failed");

    const fn label(self) -> &'static str {
        self.0
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendCleanupSkip {
    name: Option<String>,
    reason: BackendCleanupSkipReason,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendCleanupEntry {
    name: String,
    kind: BackendCleanupKind,
    bytes: u64,
    reason: BackendCleanupReason,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendCleanupReport {
    state_directory: &'static str,
    dry_run: bool,
    examined: usize,
    eligible: Vec<BackendCleanupEntry>,
    removed: Vec<BackendCleanupEntry>,
    skipped: Vec<BackendCleanupSkip>,
    reclaimed_bytes: u64,
    truncated: bool,
}

enum PidState {
    Missing,
    Valid(BackendPidRecord),
    UnsupportedVersion,
    Invalid(String),
}

struct StatePaths {
    project: PathBuf,
    directory: PathBuf,
}

pub(super) async fn run(command: BackendCommand) -> Result<ExitCode, String> {
    match command {
        BackendCommand::Status(arguments) => {
            let status = backend_status(&arguments.path, "llama-server").await?;
            render_status(&status, arguments.json)?;
            Ok(ExitCode::SUCCESS)
        }
        BackendCommand::Start(arguments) => run_start(arguments).await,
        BackendCommand::Restart(arguments) => run_restart(arguments).await,
        BackendCommand::Stop(arguments) => run_stop(arguments).await,
        BackendCommand::Logs(arguments) => run_logs(arguments).await,
        BackendCommand::Cleanup(arguments) => run_cleanup(&arguments),
    }
}

fn state_paths(project: &Path) -> Result<StatePaths, String> {
    let project =
        fs::canonicalize(project).map_err(|_| "backend project path is unavailable".to_owned())?;
    let marker = project.join(".cartograph");
    let marker_metadata = fs::symlink_metadata(&marker)
        .map_err(|_| "project is not initialized; .cartograph is missing".to_owned())?;
    if !marker_metadata.is_dir() || marker_metadata.file_type().is_symlink() {
        return Err("project .cartograph path is not a safe directory".to_owned());
    }
    let directory = marker.join(STATE_DIRECTORY);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err("backend state path is not a safe directory".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("backend state path is unavailable".to_owned()),
    }
    Ok(StatePaths { project, directory })
}

fn ensure_state_directory(paths: &StatePaths) -> Result<(), String> {
    if !paths.directory.exists() {
        fs::create_dir(&paths.directory)
            .map_err(|_| "could not create the backend state directory".to_owned())?;
    }
    let metadata = fs::symlink_metadata(&paths.directory)
        .map_err(|_| "backend state directory is unavailable".to_owned())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("backend state path is not a safe directory".to_owned());
    }
    #[cfg(unix)]
    set_private_directory(&paths.directory)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| "could not secure the backend state directory".to_owned())
}

fn build_specs(project: &Path, binary: &str) -> Result<Vec<BackendSpec>, String> {
    validate_binary(binary)?;
    let tiers = [
        (ProjectLlmTier::Summarize, "summarize"),
        (ProjectLlmTier::Local, "local"),
        (ProjectLlmTier::Ask, "ask"),
        (ProjectLlmTier::Classify, "classify"),
        (ProjectLlmTier::Embedding, "embed"),
        (ProjectLlmTier::Reranker, "rerank"),
    ];
    let mut specs = BTreeMap::<String, BackendSpec>::new();
    for (tier, label) in tiers {
        let Some(config) =
            load_exact_project_llm_tier(project, tier).map_err(|error| error.to_string())?
        else {
            continue;
        };
        let Some((key, mut spec)) = build_tier_spec(&TierSpecInput {
            tier,
            label,
            config: &config,
            binary,
        })?
        else {
            continue;
        };
        if let Some(existing) = specs.get_mut(&key) {
            if existing.model_path != spec.model_path
                || existing.args != spec.args
                || existing.command != spec.command
            {
                return Err(format!(
                    "LLM tiers sharing {} disagree on model, concurrency, or llamaServerArgs",
                    existing.endpoint
                ));
            }
            existing.labels.append(&mut spec.labels);
            existing.externally_managed |= spec.externally_managed;
        } else {
            specs.insert(key, spec);
        }
    }
    Ok(specs.into_values().collect())
}

struct TierSpecInput<'a> {
    tier: ProjectLlmTier,
    label: &'a str,
    config: &'a ProjectLlmTierConfig,
    binary: &'a str,
}

fn build_tier_spec(input: &TierSpecInput<'_>) -> Result<Option<(String, BackendSpec)>, String> {
    let &TierSpecInput {
        tier,
        label,
        config,
        binary,
    } = input;
    let model_path = PathBuf::from(config.model());
    if !model_path.is_absolute() {
        return Ok(None);
    }
    let Some((endpoint, host, port)) = local_endpoint(config.endpoint())? else {
        return Ok(None);
    };
    validate_passthrough(config.llama_server_args())?;
    let mode_args: &[&str] = match tier {
        ProjectLlmTier::Decision => return Ok(None),
        ProjectLlmTier::Embedding => &[
            "--embeddings",
            "--batch-size",
            "512",
            "--ubatch-size",
            "512",
        ],
        ProjectLlmTier::Reranker => &["--reranking"],
        ProjectLlmTier::Summarize
        | ProjectLlmTier::Local
        | ProjectLlmTier::Ask
        | ProjectLlmTier::Classify => &[],
    };
    let parallel = config.concurrency().unwrap_or(match tier {
        ProjectLlmTier::Embedding => 4,
        ProjectLlmTier::Ask => 1,
        ProjectLlmTier::Summarize
        | ProjectLlmTier::Local
        | ProjectLlmTier::Classify
        | ProjectLlmTier::Reranker => 2,
        ProjectLlmTier::Decision => return Ok(None),
    });
    let mut args = vec![
        "-m".to_owned(),
        model_path.to_string_lossy().into_owned(),
        "--host".to_owned(),
        host.clone(),
        "--port".to_owned(),
        port.to_string(),
    ];
    args.extend(mode_args.iter().map(|argument| (*argument).to_owned()));
    if !has_parallel_flag(config.llama_server_args()) {
        args.extend(["--parallel".to_owned(), parallel.to_string()]);
    }
    args.extend(config.llama_server_args().iter().cloned());
    let mode = mode_args.join("\0");
    let key = format!("{endpoint}\0{mode}");
    let id = format!("llama-{}", short_hash(&key));
    Ok(Some((
        key,
        BackendSpec {
            id,
            labels: vec![label.to_owned()],
            endpoint,
            model_path,
            host,
            port,
            parallel,
            command: binary.to_owned(),
            args,
            externally_managed: config.externally_managed(),
        },
    )))
}

fn validate_binary(binary: &str) -> Result<(), String> {
    if binary.trim().is_empty() || binary.len() > 4_096 || binary.chars().any(char::is_control) {
        Err("--bin is invalid".to_owned())
    } else {
        Ok(())
    }
}

fn validate_passthrough(arguments: &[String]) -> Result<(), String> {
    const OWNED: &[&str] = &[
        "-m",
        "--model",
        "--model-url",
        "--host",
        "--port",
        "--embeddings",
        "--embedding",
        "--rerank",
        "--reranking",
    ];
    if arguments.iter().any(|argument| {
        OWNED.iter().any(|owned| {
            argument == owned
                || argument
                    .strip_prefix(owned)
                    .is_some_and(|tail| tail.starts_with('='))
        })
    }) {
        Err(
            "llamaServerArgs cannot override Cartograph-owned model, endpoint, or mode flags"
                .to_owned(),
        )
    } else {
        Ok(())
    }
}

fn has_parallel_flag(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        matches!(argument.as_str(), "--parallel" | "-np")
            || argument.starts_with("--parallel=")
            || argument.starts_with("-np=")
    })
}

fn local_endpoint(raw: &str) -> Result<Option<(String, String, u16)>, String> {
    let url = Url::parse(raw).map_err(|_| "configured LLM endpoint is invalid".to_owned())?;
    if url.scheme() != "http" {
        return Ok(None);
    }
    let Some(raw_host) = url.host_str() else {
        return Err("configured LLM endpoint has no host".to_owned());
    };
    let host = raw_host.strip_circumfix('[', ']').unwrap_or(raw_host);
    if !is_loopback(host) {
        return Ok(None);
    }
    let port = url.port().unwrap_or(80);
    let display_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(Some((
        format!("http://{display_host}:{port}"),
        host.to_owned(),
        port,
    )))
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn valid_backend_id(value: &str) -> bool {
    value.strip_prefix("llama-").is_some_and(|digest| {
        digest.len() == 12
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_cleanup_entry_name(name: &str) -> bool {
    name.strip_suffix(".log.1")
        .or_else(|| name.strip_suffix(".json"))
        .is_some_and(valid_backend_id)
}

async fn backend_status(project: &Path, binary: &str) -> Result<BackendStatusReport, String> {
    let paths = state_paths(project)?;
    let config_specs = build_specs(&paths.project, binary)?;
    let known = config_specs
        .iter()
        .map(|spec| spec.id.as_str())
        .collect::<BTreeSet<_>>();
    let (orphans, mut warnings) = discover_orphans(&paths.directory, &known)?;
    let config_count = config_specs.len();
    let all = config_specs.into_iter().chain(orphans).collect::<Vec<_>>();
    let directory = paths.directory.clone();
    let mut rows = stream::iter(all.into_iter().enumerate())
        .map(|(index, spec)| {
            let directory = directory.clone();
            async move {
                build_status_row(
                    spec,
                    if index < config_count {
                        BackendOrigin::Config
                    } else {
                        BackendOrigin::Orphan
                    },
                    &directory,
                )
                .await
            }
        })
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
    rows.sort_by(|left, right| left.spec.id.cmp(&right.spec.id));
    warnings.sort();
    let unmanaged_reason = rows.is_empty().then(|| {
        "No manageable local llama-server tiers. Only loopback openai-compat tiers with absolute GGUF model paths are process-managed.".to_owned()
    });
    Ok(BackendStatusReport {
        project_path: paths.project,
        state_directory: paths.directory,
        rows,
        warnings,
        unmanaged_reason,
    })
}

async fn build_status_row(
    spec: BackendSpec,
    origin: BackendOrigin,
    directory: &Path,
) -> BackendRow {
    let pid_file_path = directory.join(format!("{}.json", spec.id));
    let log_path = directory.join(format!("{}.log", spec.id));
    let pid_state = read_pid_record(&pid_file_path);
    let (pid_record, state_error) = match pid_state {
        PidState::Missing => (None, None),
        PidState::Valid(record) => (Some(record), None),
        PidState::UnsupportedVersion => {
            (None, Some("state file version is unsupported".to_owned()))
        }
        PidState::Invalid(message) => (None, Some(message)),
    };
    let pid_alive = pid_record
        .as_ref()
        .is_some_and(|record| process_alive(record.pid));
    let endpoint_reachable =
        probe_openai_compatible_endpoint(&spec.endpoint, ENDPOINT_PROBE_TIMEOUT)
            .await
            .is_ok_and(|probe| probe.openai_compatible);
    let model_exists = safe_regular_file(&spec.model_path);
    let config_drift = pid_record
        .as_ref()
        .is_some_and(|record| record.command != spec.command || record.args != spec.args);
    let state = if state_error.is_some() {
        BackendState::InvalidState
    } else if spec.externally_managed {
        BackendState::External
    } else if !model_exists {
        BackendState::MissingModel
    } else if pid_alive && endpoint_reachable {
        BackendState::Running
    } else if pid_alive {
        BackendState::Starting
    } else if endpoint_reachable {
        BackendState::External
    } else {
        BackendState::Stopped
    };
    BackendRow {
        spec,
        origin,
        pid_file_path,
        log_path,
        pid_record,
        process_health: BackendProcessHealth {
            pid_alive,
            endpoint_reachable,
        },
        artifact_health: BackendArtifactHealth {
            model_exists,
            config_drift,
        },
        state,
        state_error,
    }
}

fn discover_orphans(
    directory: &Path,
    known: &BTreeSet<&str>,
) -> Result<(Vec<BackendSpec>, Vec<String>), String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(_) => return Err("backend state directory cannot be read".to_owned()),
    };
    let mut orphans = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = 0_usize;
    for entry in entries.take(MAXIMUM_STATE_ENTRIES + 1) {
        seen = seen.saturating_add(1);
        if seen > MAXIMUM_STATE_ENTRIES {
            warnings.push(format!(
                "backend state directory exceeds the {MAXIMUM_STATE_ENTRIES}-entry inspection bound"
            ));
            break;
        }
        let entry = entry.map_err(|_| "backend state directory cannot be read".to_owned())?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if !valid_backend_id(id) {
            warnings.push("ignored backend state with an invalid entry name".to_owned());
            continue;
        }
        if known.contains(id) {
            continue;
        }
        match read_pid_record(&entry.path()) {
            PidState::Valid(record) => match orphan_spec(id, &record) {
                Ok(spec) => orphans.push(spec),
                Err(message) => warnings.push(format!("ignored orphan state {name}: {message}")),
            },
            PidState::Invalid(message) => {
                warnings.push(format!("invalid backend state {name}: {message}"));
            }
            PidState::UnsupportedVersion => {
                warnings.push(format!("preserved unsupported backend state {name}"));
            }
            PidState::Missing => {}
        }
    }
    Ok((orphans, warnings))
}

fn orphan_spec(id: &str, record: &BackendPidRecord) -> Result<BackendSpec, String> {
    let Some((endpoint, host, port)) = local_endpoint(&record.endpoint)? else {
        return Err("orphan endpoint is not loopback HTTP".to_owned());
    };
    Ok(BackendSpec {
        id: id.to_owned(),
        labels: record.labels.clone(),
        endpoint,
        model_path: record.model_path.clone(),
        host,
        port,
        parallel: 0,
        command: record.command.clone(),
        args: record.args.clone(),
        externally_managed: false,
    })
}

fn read_pid_record(path: &Path) -> PidState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PidState::Missing,
        Err(_) => return PidState::Invalid("state file is unavailable".to_owned()),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAXIMUM_STATE_FILE_BYTES
    {
        return PidState::Invalid("state file is unsafe or oversized".to_owned());
    }
    let mut bytes = Vec::new();
    let result = File::open(path).and_then(|file| {
        file.take(MAXIMUM_STATE_FILE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
    });
    if result.is_err() || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAXIMUM_STATE_FILE_BYTES
    {
        return PidState::Invalid("state file cannot be read safely".to_owned());
    }
    let Ok(version) = serde_json::from_slice::<BackendPidVersion>(&bytes) else {
        return PidState::Invalid("state file is malformed".to_owned());
    };
    if version.schema_version != u64::from(PID_SCHEMA_VERSION) {
        return PidState::UnsupportedVersion;
    }
    let Ok(record) = serde_json::from_slice::<BackendPidRecord>(&bytes) else {
        return PidState::Invalid("state file is malformed".to_owned());
    };
    if !valid_pid_record(&record) {
        return PidState::Invalid("state file fields are invalid".to_owned());
    }
    PidState::Valid(record)
}

fn valid_pid_record(record: &BackendPidRecord) -> bool {
    record.schema_version == PID_SCHEMA_VERSION
        && record.pid > 0
        && !record.command.is_empty()
        && record.command.len() <= MAXIMUM_PID_COMMAND_BYTES
        && record.args.len() <= MAXIMUM_PID_ARGUMENTS
        && record.args.iter().all(|argument| {
            !argument.is_empty()
                && argument.len() <= MAXIMUM_PID_COMMAND_BYTES
                && !argument.chars().any(char::is_control)
        })
        && !record.endpoint.is_empty()
        && record.labels.len() <= MAXIMUM_PID_LABELS
        && !record.labels.is_empty()
        && record.labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= MAXIMUM_PID_LABEL_BYTES
                && !label.chars().any(char::is_control)
        })
        && record.model_path.is_absolute()
        && record.log_path.is_absolute()
}

fn safe_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let expected_pid = pid.to_string();
        ProcessCommand::new("ps")
            .args(["-p", &expected_pid, "-o", "pid=,stat="])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| {
                if !output.status.success() || output.stdout.len() > 64 {
                    return false;
                }
                let rendered = String::from_utf8_lossy(&output.stdout);
                let mut fields = rendered.split_whitespace();
                fields.next() == Some(expected_pid.as_str())
                    && fields.next().is_some_and(|state| !state.starts_with('Z'))
                    && fields.next().is_none()
            })
    }
    #[cfg(windows)]
    {
        ProcessCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && output.stdout.len() <= MAXIMUM_PROCESS_OUTPUT_BYTES
                    && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
            })
    }
}

fn disposition(row: &BackendRow, reason: impl Into<String>) -> ActionDisposition {
    ActionDisposition {
        labels: row.spec.labels.clone(),
        endpoint: row.spec.endpoint.clone(),
        reason: reason.into(),
    }
}

async fn run_start(arguments: StartArguments) -> Result<ExitCode, String> {
    let before = backend_status(&arguments.path, &arguments.binary).await?;
    let paths = state_paths(&arguments.path)?;
    let mut changed = Vec::new();
    let mut skipped = Vec::new();
    if !arguments.dry_run {
        ensure_state_directory(&paths)?;
    }
    let mut eligible = Vec::new();
    for row in &before.rows {
        if let Some(reason) = start_skip_reason(row) {
            skipped.push(disposition(row, reason));
            continue;
        }
        if arguments.dry_run {
            changed.push(disposition(row, "would start"));
            continue;
        }
        eligible.push(row.clone());
    }
    let started = stream::iter(eligible)
        .map(|row| async move {
            let result = spawn_row(&row).await;
            (row, result)
        })
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
    for (row, result) in started {
        match result {
            Ok(pid) => changed.push(disposition(&row, format!("started pid {pid}"))),
            Err(error) => skipped.push(disposition(&row, error)),
        }
    }
    let status = if arguments.dry_run {
        before
    } else {
        backend_status(&arguments.path, &arguments.binary).await?
    };
    let failed = no_usable_backends(&status) || (!arguments.dry_run && changed.is_empty());
    let report = BackendActionReport {
        status,
        changed,
        skipped,
        dry_run: arguments.dry_run,
    };
    render_action("start", &report, arguments.json)?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn start_skip_reason(row: &BackendRow) -> Option<String> {
    if row.origin == BackendOrigin::Orphan {
        return Some("orphaned state must be reclaimed with backend stop".to_owned());
    }
    if row.state_error.is_some() {
        return Some("invalid pid state must be inspected manually".to_owned());
    }
    if row.spec.externally_managed {
        return Some("tier is explicitly externally managed".to_owned());
    }
    if !row.artifact_health.model_exists {
        return Some("configured GGUF model is missing or unsafe".to_owned());
    }
    if row.process_health.pid_alive {
        return Some(format!(
            "already running as pid {}",
            row.pid_record.as_ref().map_or(0, |record| record.pid)
        ));
    }
    if row.process_health.endpoint_reachable {
        return Some("endpoint is already held by an external process".to_owned());
    }
    None
}

async fn spawn_row(row: &BackendRow) -> Result<u32, String> {
    let log = open_log_file(&row.log_path)?;
    let stderr = log
        .try_clone()
        .map_err(|_| "could not clone backend log handle".to_owned())?;
    let mut command = ProcessCommand::new(&row.spec.command);
    command
        .args(&row.spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .map_err(|_| "llama-server process could not be started".to_owned())?;
    let pid = child.id();
    tokio::time::sleep(START_CONFIRMATION).await;
    if let Some(status) = child
        .try_wait()
        .map_err(|_| "could not confirm llama-server startup".to_owned())?
    {
        return Err(format!(
            "llama-server exited during startup with status {status}"
        ));
    }
    if let Err(error) = write_pid_record(row, pid) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(pid)
}

fn open_log_file(path: &Path) -> Result<File, String> {
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err("backend log target is not a safe regular file".to_owned());
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("backend log target is unavailable".to_owned()),
    };
    if existing.is_some_and(|metadata| metadata.len() >= MAXIMUM_BACKEND_LOG_BYTES) {
        rotate_backend_log(path)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|_| "backend log cannot be opened".to_owned())
}

fn rotate_backend_log(path: &Path) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "backend log path is invalid".to_owned())?;
    let rotated = path.with_file_name(format!("{file_name}{ROTATED_LOG_SUFFIX}"));
    match fs::symlink_metadata(&rotated) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err("rotated backend log target is unsafe".to_owned());
        }
        Ok(_) => fs::remove_file(&rotated)
            .map_err(|_| "prior rotated backend log cannot be removed".to_owned())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("rotated backend log target is unavailable".to_owned()),
    }
    fs::rename(path, rotated).map_err(|_| "backend log cannot be rotated".to_owned())
}

fn write_pid_record(row: &BackendRow, pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Err("child process did not report a valid pid".to_owned());
    }
    if fs::symlink_metadata(&row.pid_file_path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err("backend pid target is unsafe".to_owned());
    }
    let record = BackendPidRecord {
        schema_version: PID_SCHEMA_VERSION,
        pid,
        started_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            }),
        command: row.spec.command.clone(),
        args: row.spec.args.clone(),
        endpoint: row.spec.endpoint.clone(),
        model_path: row.spec.model_path.clone(),
        labels: row.spec.labels.clone(),
        log_path: row.log_path.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&record)
        .map_err(|_| "backend pid state could not be serialized".to_owned())?;
    bytes.push(b'\n');
    let mut temporary = NamedTempFile::new_in(
        row.pid_file_path
            .parent()
            .ok_or_else(|| "backend pid target has no parent".to_owned())?,
    )
    .map_err(|_| "backend pid state could not be staged".to_owned())?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| "backend pid state could not be written".to_owned())?;
    #[cfg(unix)]
    set_private_file(temporary.as_file())?;
    temporary
        .persist(&row.pid_file_path)
        .map(|_| ())
        .map_err(|_| "backend pid state could not be published".to_owned())
}

#[cfg(unix)]
fn set_private_file(file: &File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| "backend state permissions could not be secured".to_owned())
}

async fn run_stop(arguments: StopArguments) -> Result<ExitCode, String> {
    let before = backend_status(&arguments.path, "llama-server").await?;
    let mut changed = Vec::new();
    let mut skipped = Vec::new();
    for row in &before.rows {
        match stop_row(row, arguments.force).await {
            Ok(Some(reason)) => changed.push(disposition(row, reason)),
            Ok(None) => skipped.push(disposition(row, "not managed or not running")),
            Err(error) => skipped.push(disposition(row, error)),
        }
    }
    let status = backend_status(&arguments.path, "llama-server").await?;
    let report = BackendActionReport {
        status,
        changed,
        skipped,
        dry_run: false,
    };
    render_action("stop", &report, arguments.json)?;
    Ok(ExitCode::SUCCESS)
}

async fn stop_row(row: &BackendRow, force: bool) -> Result<Option<String>, String> {
    if row.spec.externally_managed {
        return Ok(None);
    }
    if row.state_error.is_some() {
        return Err("invalid pid state; refusing to signal an unverified process".to_owned());
    }
    let Some(record) = &row.pid_record else {
        return Ok(None);
    };
    if !row.process_health.pid_alive {
        remove_pid_file(&row.pid_file_path)?;
        return Ok(Some(format!("removed stale pid state for {}", record.pid)));
    }
    if !process_identity_matches(record) {
        return Err(format!(
            "pid {} is alive but its command identity does not match the recorded backend",
            record.pid
        ));
    }
    signal_process(record.pid, false)?;
    let mut exited = wait_for_exit(record.pid, STOP_GRACE).await;
    if !exited && force {
        signal_process(record.pid, true)?;
        exited = wait_for_exit(record.pid, STOP_GRACE).await;
    }
    if !exited {
        return Err(format!(
            "pid {} did not exit; retry with --force or inspect it manually",
            record.pid
        ));
    }
    remove_pid_file(&row.pid_file_path)?;
    Ok(Some(format!("stopped pid {}", record.pid)))
}

fn process_identity_matches(record: &BackendPidRecord) -> bool {
    let Some(command_line) = process_command_line(record.pid) else {
        return false;
    };
    let binary = Path::new(&record.command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&record.command);
    command_line.contains(binary)
        && command_line.contains(&record.model_path.to_string_lossy().into_owned())
        && Url::parse(&record.endpoint)
            .ok()
            .and_then(|url| url.port_or_known_default())
            .is_some_and(|port| command_line.contains(&port.to_string()))
}

fn process_command_line(pid: u32) -> Option<String> {
    #[cfg(unix)]
    let output = ProcessCommand::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    #[cfg(windows)]
    let output = ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}').CommandLine"),
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAXIMUM_PROCESS_OUTPUT_BYTES {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .filter(|line| !line.trim().is_empty())
}

fn signal_process(pid: u32, force: bool) -> Result<(), String> {
    #[cfg(unix)]
    let status = ProcessCommand::new("kill")
        .args([if force { "-KILL" } else { "-TERM" }, &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(windows)]
    let status = {
        let mut command = ProcessCommand::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/T"]);
        if force {
            command.arg("/F");
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    if status.is_ok_and(|status| status.success()) {
        Ok(())
    } else {
        Err(format!("could not signal verified backend pid {pid}"))
    }
}

async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(STOP_POLL).await;
    }
    !process_alive(pid)
}

fn remove_pid_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "backend pid state cannot be inspected".to_owned())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("backend pid state is unsafe; refusing removal".to_owned());
    }
    fs::remove_file(path).map_err(|_| "backend pid state cannot be removed".to_owned())
}

#[derive(Clone, Copy)]
struct RestartPolicy {
    force: bool,
    dry_run: bool,
}

enum RestartOutcome {
    Changed(ActionDisposition),
    Skipped(ActionDisposition),
}

fn restart_skip_reason(row: &BackendRow) -> Option<String> {
    if row.origin == BackendOrigin::Orphan {
        Some("orphaned state must be reclaimed with backend stop".to_owned())
    } else if row.spec.externally_managed
        || (row.process_health.endpoint_reachable && row.pid_record.is_none())
    {
        Some(format!(
            "external process; relaunch manually with {}",
            render_command(&row.spec)
        ))
    } else if !row.artifact_health.model_exists || row.state_error.is_some() {
        Some("backend model or pid state is invalid".to_owned())
    } else {
        None
    }
}

async fn restart_row(row: &BackendRow, policy: RestartPolicy) -> RestartOutcome {
    if let Some(reason) = restart_skip_reason(row) {
        return RestartOutcome::Skipped(disposition(row, reason));
    }
    if policy.dry_run {
        return RestartOutcome::Changed(disposition(row, "would restart"));
    }
    if row.pid_record.is_some()
        && let Err(error) = stop_row(row, policy.force).await
    {
        return RestartOutcome::Skipped(disposition(row, error));
    }
    match spawn_row(row).await {
        Ok(pid) => RestartOutcome::Changed(disposition(row, format!("restarted as pid {pid}"))),
        Err(error) => RestartOutcome::Skipped(disposition(row, error)),
    }
}

fn row_matches_tier(row: &BackendRow, tier: Option<BackendTier>) -> bool {
    tier.is_none_or(|tier| row.spec.labels.iter().any(|label| label == tier.label()))
}

async fn run_restart(arguments: RestartArguments) -> Result<ExitCode, String> {
    let before = backend_status(&arguments.path, &arguments.binary).await?;
    let paths = state_paths(&arguments.path)?;
    let mut changed = Vec::new();
    let mut skipped = Vec::new();
    if !arguments.dry_run {
        ensure_state_directory(&paths)?;
    }
    let policy = RestartPolicy {
        force: arguments.force,
        dry_run: arguments.dry_run,
    };
    for row in before
        .rows
        .iter()
        .filter(|row| row_matches_tier(row, arguments.tier))
    {
        match restart_row(row, policy).await {
            RestartOutcome::Changed(result) => changed.push(result),
            RestartOutcome::Skipped(result) => skipped.push(result),
        }
    }
    let status = if arguments.dry_run {
        before
    } else {
        backend_status(&arguments.path, &arguments.binary).await?
    };
    let failed = no_usable_backends(&status) || changed.is_empty();
    let report = BackendActionReport {
        status,
        changed,
        skipped,
        dry_run: arguments.dry_run,
    };
    render_action("restart", &report, arguments.json)?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn run_logs(arguments: LogsArguments) -> Result<ExitCode, String> {
    let status = backend_status(&arguments.path, "llama-server").await?;
    let selected = status.rows.iter().filter(|row| {
        arguments
            .tier
            .is_none_or(|tier| row.spec.labels.iter().any(|label| label == tier.label()))
    });
    let logs = selected
        .map(|row| match tail_log(&row.log_path, arguments.lines) {
            Ok(Some(content)) => BackendLogEntry {
                labels: row.spec.labels.clone(),
                path: row.log_path.clone(),
                exists: true,
                content,
                error: None,
            },
            Ok(None) => BackendLogEntry {
                labels: row.spec.labels.clone(),
                path: row.log_path.clone(),
                exists: false,
                content: String::new(),
                error: None,
            },
            Err(error) => BackendLogEntry {
                labels: row.spec.labels.clone(),
                path: row.log_path.clone(),
                exists: false,
                content: String::new(),
                error: Some(error),
            },
        })
        .collect::<Vec<_>>();
    let failed = arguments.tier.is_some() && logs.is_empty();
    let report = BackendLogsReport { status, logs };
    if arguments.json {
        print_json(&report)?;
    } else {
        render_logs(&report);
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[derive(Clone)]
struct PlannedBackendCleanup {
    path: PathBuf,
    entry: BackendCleanupEntry,
    modified: SystemTime,
}

#[derive(Default)]
struct BackendCleanupScan {
    planned: Vec<PlannedBackendCleanup>,
    skipped: Vec<BackendCleanupSkip>,
    examined: usize,
    truncated: bool,
}

#[derive(Default)]
struct BackendCleanupExecution {
    removed: Vec<BackendCleanupEntry>,
    reclaimed_bytes: u64,
}

fn run_cleanup(arguments: &CleanupArguments) -> Result<ExitCode, String> {
    let report = backend_cleanup_report(arguments)?;
    if arguments.json {
        print_json(&report)?;
    } else {
        render_cleanup(&report);
    }
    Ok(ExitCode::SUCCESS)
}

fn backend_cleanup_report(arguments: &CleanupArguments) -> Result<BackendCleanupReport, String> {
    validate_cleanup_confirmation(arguments)?;
    let paths = state_paths(&arguments.path)?;
    let minimum_age = Duration::from_secs(u64::from(arguments.minimum_age_hours) * 60 * 60);
    let mut scan = scan_backend_cleanup(&paths.directory, minimum_age)?;
    limit_cleanup_candidates(&mut scan, arguments.maximum_deletions);
    let eligible = scan
        .planned
        .iter()
        .map(|candidate| candidate.entry.clone())
        .collect();
    let execution = if arguments.apply {
        remove_cleanup_candidates(scan.planned, minimum_age, &mut scan.skipped)
    } else {
        BackendCleanupExecution::default()
    };
    Ok(BackendCleanupReport {
        state_directory: PUBLIC_STATE_DIRECTORY,
        dry_run: !arguments.apply,
        examined: scan.examined,
        eligible,
        removed: execution.removed,
        skipped: scan.skipped,
        reclaimed_bytes: execution.reclaimed_bytes,
        truncated: scan.truncated,
    })
}

fn validate_cleanup_confirmation(arguments: &CleanupArguments) -> Result<(), String> {
    if arguments.apply && arguments.confirm.as_deref() != Some(CLEANUP_CONFIRMATION) {
        return Err(format!(
            "backend cleanup --apply requires --confirm {CLEANUP_CONFIRMATION}"
        ));
    }
    Ok(())
}

fn scan_backend_cleanup(
    directory: &Path,
    minimum_age: Duration,
) -> Result<BackendCleanupScan, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackendCleanupScan::default());
        }
        Err(_) => return Err("backend state directory cannot be read".to_owned()),
    };
    let mut scan = BackendCleanupScan::default();
    for result in entries.take(MAXIMUM_STATE_ENTRIES + 1) {
        if scan.examined >= MAXIMUM_STATE_ENTRIES {
            scan.truncated = true;
            break;
        }
        scan.examined = scan.examined.saturating_add(1);
        match inspect_cleanup_entry(result, minimum_age) {
            Ok(Some(candidate)) => scan.planned.push(candidate),
            Err(reason) => scan.skipped.push(reason),
            Ok(None) => {}
        }
    }
    scan.planned.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.entry.name.cmp(&right.entry.name))
    });
    Ok(scan)
}

fn inspect_cleanup_entry(
    result: std::io::Result<fs::DirEntry>,
    minimum_age: Duration,
) -> Result<Option<PlannedBackendCleanup>, BackendCleanupSkip> {
    let entry = result.map_err(|_| BackendCleanupSkip {
        name: None,
        reason: BackendCleanupSkipReason::ENTRY_UNREADABLE,
    })?;
    let path = entry.path();
    let name = entry
        .file_name()
        .into_string()
        .map_err(|_| BackendCleanupSkip {
            name: None,
            reason: BackendCleanupSkipReason::NON_UNICODE_ENTRY,
        })?;
    if !is_cleanup_file_name(&name) {
        return Ok(None);
    }
    if !valid_cleanup_entry_name(&name) {
        return Err(BackendCleanupSkip {
            name: None,
            reason: BackendCleanupSkipReason::INVALID_ENTRY_NAME,
        });
    }
    let Some(metadata) = cleanup_entry_metadata(&path, &name)? else {
        return Ok(None);
    };
    let Some(modified) = cleanup_entry_modified(&metadata, &name, minimum_age)? else {
        return Ok(None);
    };
    preserve_unsupported_pid_state(&path, &name)?;
    let Some((kind, reason)) = classify_cleanup_entry(&name, &path) else {
        return Ok(None);
    };
    Ok(Some(PlannedBackendCleanup {
        path,
        entry: BackendCleanupEntry {
            name,
            kind,
            bytes: metadata.len(),
            reason,
        },
        modified,
    }))
}

fn is_cleanup_file_name(name: &str) -> bool {
    has_ascii_suffix(name, ".log.1") || has_ascii_suffix(name, ".json")
}

fn has_ascii_suffix(value: &str, suffix: &str) -> bool {
    value
        .get(value.len().saturating_sub(suffix.len())..)
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(suffix))
}

fn cleanup_entry_metadata(
    path: &Path,
    name: &str,
) -> Result<Option<fs::Metadata>, BackendCleanupSkip> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(metadata))
        }
        Ok(_) => Err(BackendCleanupSkip {
            name: Some(name.to_owned()),
            reason: BackendCleanupSkipReason::UNSAFE_ENTRY_TYPE,
        }),
        Err(_) => Ok(None),
    }
}

fn cleanup_entry_modified(
    metadata: &fs::Metadata,
    name: &str,
    minimum_age: Duration,
) -> Result<Option<SystemTime>, BackendCleanupSkip> {
    match metadata.modified() {
        Ok(modified) if old_enough(modified, minimum_age) => Ok(Some(modified)),
        Ok(_) => Ok(None),
        Err(_) => Err(BackendCleanupSkip {
            name: Some(name.to_owned()),
            reason: BackendCleanupSkipReason::MODIFICATION_TIME_UNAVAILABLE,
        }),
    }
}

fn preserve_unsupported_pid_state(path: &Path, name: &str) -> Result<(), BackendCleanupSkip> {
    if has_ascii_suffix(name, ".json")
        && matches!(read_pid_record(path), PidState::UnsupportedVersion)
    {
        return Err(BackendCleanupSkip {
            name: Some(name.to_owned()),
            reason: BackendCleanupSkipReason::UNSUPPORTED_STATE_VERSION,
        });
    }
    Ok(())
}

fn classify_cleanup_entry(
    name: &str,
    path: &Path,
) -> Option<(BackendCleanupKind, BackendCleanupReason)> {
    if has_ascii_suffix(name, ".log.1") {
        return Some((
            BackendCleanupKind::RotatedLog,
            BackendCleanupReason::BoundedRotatedLog,
        ));
    }
    if has_ascii_suffix(name, ".json") && matches!(read_pid_record(path), PidState::Invalid(_)) {
        return Some((
            BackendCleanupKind::InvalidPidState,
            BackendCleanupReason::StaleInvalidPidState,
        ));
    }
    None
}

fn limit_cleanup_candidates(scan: &mut BackendCleanupScan, maximum_deletions: u16) {
    let maximum = usize::from(maximum_deletions);
    if scan.planned.len() > maximum {
        scan.planned.truncate(maximum);
        scan.truncated = true;
    }
}

fn remove_cleanup_candidates(
    candidates: Vec<PlannedBackendCleanup>,
    minimum_age: Duration,
    skipped: &mut Vec<BackendCleanupSkip>,
) -> BackendCleanupExecution {
    let mut execution = BackendCleanupExecution::default();
    for candidate in candidates {
        if !cleanup_candidate_still_safe(&candidate, minimum_age) {
            skipped.push(BackendCleanupSkip {
                name: Some(candidate.entry.name.clone()),
                reason: BackendCleanupSkipReason::CANDIDATE_CHANGED,
            });
            continue;
        }
        match fs::remove_file(&candidate.path) {
            Ok(()) => {
                execution.reclaimed_bytes = execution
                    .reclaimed_bytes
                    .saturating_add(candidate.entry.bytes);
                execution.removed.push(candidate.entry);
            }
            Err(_) => skipped.push(BackendCleanupSkip {
                name: Some(candidate.entry.name),
                reason: BackendCleanupSkipReason::REMOVAL_FAILED,
            }),
        }
    }
    execution
}

fn old_enough(modified: SystemTime, minimum_age: Duration) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age >= minimum_age)
}

fn cleanup_candidate_still_safe(candidate: &PlannedBackendCleanup, minimum_age: Duration) -> bool {
    let Ok(metadata) = fs::symlink_metadata(&candidate.path) else {
        return false;
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() != candidate.entry.bytes
        || metadata.modified().ok() != Some(candidate.modified)
        || !old_enough(candidate.modified, minimum_age)
    {
        return false;
    }
    candidate.entry.kind != BackendCleanupKind::InvalidPidState
        || matches!(read_pid_record(&candidate.path), PidState::Invalid(_))
}

fn tail_log(path: &Path, lines: u16) -> Result<Option<String>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("log file is unavailable".to_owned()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("log path is not a safe regular file".to_owned());
    }
    let start = metadata.len().saturating_sub(MAXIMUM_LOG_TAIL_BYTES);
    let mut file = File::open(path).map_err(|_| "log file cannot be opened".to_owned())?;
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "log file cannot be seeked".to_owned())?;
    let mut bytes = Vec::new();
    file.take(MAXIMUM_LOG_TAIL_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "log file cannot be read".to_owned())?;
    if start > 0
        && let Some(newline) = bytes.iter().position(|byte| *byte == b'\n')
    {
        bytes.drain(..=newline);
    }
    let text = String::from_utf8_lossy(&bytes);
    let selected = text
        .lines()
        .rev()
        .take(usize::from(lines))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some(selected))
}

fn no_usable_backends(status: &BackendStatusReport) -> bool {
    status.rows.is_empty()
        || status.rows.iter().all(|row| {
            matches!(
                row.state,
                BackendState::MissingModel | BackendState::InvalidState
            )
        })
}

fn render_status(status: &BackendStatusReport, json: bool) -> Result<(), String> {
    if json {
        return print_json(status);
    }
    println!("## cartograph backend status\n");
    render_rows(status);
    Ok(())
}

fn render_action(action: &str, report: &BackendActionReport, json: bool) -> Result<(), String> {
    if json {
        return print_json(report);
    }
    println!("## cartograph backend {action}\n");
    if report.dry_run {
        println!("_Dry run: no process state changed._\n");
    }
    for changed in &report.changed {
        println!(
            "✓ {} at {} — {}",
            changed.labels.join("/"),
            changed.endpoint,
            changed.reason
        );
    }
    for skipped in &report.skipped {
        println!(
            "○ {} at {} — {}",
            skipped.labels.join("/"),
            skipped.endpoint,
            skipped.reason
        );
    }
    if !report.changed.is_empty() || !report.skipped.is_empty() {
        println!();
    }
    render_rows(&report.status);
    Ok(())
}

fn render_rows(status: &BackendStatusReport) {
    if status.rows.is_empty() {
        println!(
            "_No managed backend processes._ {}",
            status.unmanaged_reason.as_deref().unwrap_or_default()
        );
    }
    for row in &status.rows {
        let marker = match row.state {
            BackendState::Running | BackendState::External => "✓",
            BackendState::MissingModel | BackendState::InvalidState => "✗",
            BackendState::Starting | BackendState::Stopped => "○",
        };
        let pid = row
            .pid_record
            .as_ref()
            .map(|record| format!(" pid={}", record.pid))
            .unwrap_or_default();
        println!(
            "{marker} **{}** — {:?} at {}{pid}",
            row.spec.labels.join("/"),
            row.state,
            row.spec.endpoint
        );
        println!("  model: {}", row.spec.model_path.display());
        println!("  log: {}", row.log_path.display());
        println!("  command: {}", render_command(&row.spec));
        if row.artifact_health.config_drift {
            println!("  ⚠ running arguments differ from current config; use backend restart");
        }
        if let Some(error) = &row.state_error {
            println!("  ✗ state: {error}");
        }
    }
    for warning in &status.warnings {
        println!("⚠ {warning}");
    }
}

fn render_logs(report: &BackendLogsReport) {
    println!("## cartograph backend logs\n");
    if report.logs.is_empty() {
        println!("_No backend matched the requested tier._");
    }
    for entry in &report.logs {
        println!(
            "### {} — {}\n",
            entry.labels.join("/"),
            entry.path.display()
        );
        if let Some(error) = &entry.error {
            println!("_Could not read log: {error}_\n");
        } else if !entry.exists || entry.content.is_empty() {
            println!("_No log output yet._\n");
        } else {
            println!("```text\n{}\n```\n", entry.content);
        }
    }
}

fn render_cleanup(report: &BackendCleanupReport) {
    println!("## cartograph backend cleanup\n");
    if report.dry_run {
        println!("_Dry run: no files changed._\n");
    }
    println!(
        "Inspected {} bounded state entries; {} eligible, {} removed, {} bytes reclaimed.",
        report.examined,
        report.eligible.len(),
        report.removed.len(),
        report.reclaimed_bytes
    );
    for entry in if report.dry_run {
        &report.eligible
    } else {
        &report.removed
    } {
        let action = if report.dry_run {
            "would remove"
        } else {
            "removed"
        };
        println!(
            "- {action} {} ({} bytes, {:?}): {}",
            entry.name,
            entry.bytes,
            entry.kind,
            cleanup_reason_label(entry.reason)
        );
    }
    for warning in &report.skipped {
        println!(
            "- preserved {} ({})",
            warning.name.as_deref().unwrap_or("unidentified entry"),
            warning.reason.label()
        );
    }
    if report.truncated {
        println!("- more eligible entries remain beyond this bounded batch");
    }
}

const fn cleanup_reason_label(reason: BackendCleanupReason) -> &'static str {
    match reason {
        BackendCleanupReason::BoundedRotatedLog => "bounded rotated backend log",
        BackendCleanupReason::StaleInvalidPidState => {
            "stale invalid pid state that cannot own a verified process"
        }
    }
}

fn render_command(spec: &BackendSpec) -> String {
    std::iter::once(spec.command.as_str())
        .chain(spec.args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_./:=+-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

fn print_json(value: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|_| "backend report could not be serialized".to_owned())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    const EMBED_ENDPOINT: &str = "http://127.0.0.1:8080";
    const CHAT_ENDPOINT: &str = "http://127.0.0.1:8081";
    const LOCALHOST_V1_ENDPOINT: &str = "http://localhost:8080/v1";
    const LOCALHOST_ENDPOINT: &str = "http://localhost:8080";
    const IPV6_ENDPOINT: &str = "http://[::1]:8081";
    const SECURE_LOCALHOST_ENDPOINT: &str = "https://localhost:8080";
    const REMOTE_ENDPOINT: &str = "http://example.com:8080";
    const UNREACHABLE_ENDPOINT: &str = "http://127.0.0.1:65534";
    const FIXTURE_BACKEND_ID: &str = "llama-0123456789ab";

    #[test]
    fn managed_passthrough_cannot_override_identity_flags() {
        assert!(validate_passthrough(&["--cache-ram".to_owned(), "1024".to_owned()]).is_ok());
        assert!(validate_passthrough(&["--port=9999".to_owned()]).is_err());
        assert!(validate_passthrough(&["-m".to_owned(), "other.gguf".to_owned()]).is_err());
    }

    #[test]
    fn shell_rendering_is_diagnostic_only_and_quotes_spaces() {
        assert_eq!(shell_quote("llama-server"), "llama-server");
        assert_eq!(shell_quote("model one.gguf"), "'model one.gguf'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn shared_chat_tiers_merge_and_any_external_declaration_wins() {
        let root = tempdir().unwrap_or_else(|error| panic!("fixture root failed: {error}"));
        let marker = root.path().join(".cartograph");
        fs::create_dir(&marker).unwrap_or_else(|error| panic!("fixture marker failed: {error}"));
        let chat_model = root.path().join("chat.gguf");
        let embed_model = root.path().join("embed.gguf");
        fs::write(&chat_model, b"fixture")
            .unwrap_or_else(|error| panic!("chat model fixture failed: {error}"));
        fs::write(&embed_model, b"fixture")
            .unwrap_or_else(|error| panic!("embed model fixture failed: {error}"));
        let config = serde_json::json!({
            "llm": {
                "summarizeLlm": {
                    "provider": "openai-compat",
                    "endpoint": CHAT_ENDPOINT,
                    "model": chat_model,
                    "concurrency": 2
                },
                "localLlm": {
                    "provider": "openai-compat",
                    "endpoint": CHAT_ENDPOINT,
                    "model": chat_model,
                    "concurrency": 2,
                    "externallyManaged": true
                },
                "embeddingLlm": {
                    "provider": "openai-compat",
                    "endpoint": EMBED_ENDPOINT,
                    "model": embed_model,
                    "concurrency": 4
                }
            }
        });
        fs::write(
            marker.join("config.json"),
            serde_json::to_vec(&config)
                .unwrap_or_else(|error| panic!("config fixture encode failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("config fixture write failed: {error}"));

        let specs = build_specs(root.path(), "llama-server")
            .unwrap_or_else(|error| panic!("backend specs failed: {error}"));
        assert_eq!(specs.len(), 2);
        let chat = specs
            .iter()
            .find(|spec| spec.port == 8081)
            .unwrap_or_else(|| panic!("chat spec missing"));
        assert_eq!(chat.labels, ["summarize", "local"]);
        assert!(chat.externally_managed);
        let embedding = specs
            .iter()
            .find(|spec| spec.port == 8080)
            .unwrap_or_else(|| panic!("embedding spec missing"));
        assert!(
            embedding
                .args
                .iter()
                .any(|argument| argument == "--embeddings")
        );
    }

    #[tokio::test]
    async fn empty_initialized_project_commands_are_bounded_and_do_not_spawn_processes() {
        let root = tempdir().unwrap_or_else(|error| panic!("fixture root failed: {error}"));
        fs::create_dir(root.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("fixture marker failed: {error}"));

        assert_eq!(
            run(BackendCommand::Status(StatusArguments {
                path: root.path().to_path_buf(),
                json: true,
            }))
            .await,
            Ok(ExitCode::SUCCESS)
        );
        assert_eq!(
            run(BackendCommand::Start(StartArguments {
                path: root.path().to_path_buf(),
                binary: "llama-server".to_owned(),
                dry_run: true,
                json: false,
            }))
            .await,
            Ok(ExitCode::FAILURE)
        );
        assert_eq!(
            run(BackendCommand::Restart(RestartArguments {
                path: root.path().to_path_buf(),
                binary: "llama-server".to_owned(),
                tier: None,
                force: false,
                dry_run: true,
                json: true,
            }))
            .await,
            Ok(ExitCode::FAILURE)
        );
        assert_eq!(
            run(BackendCommand::Stop(StopArguments {
                path: root.path().to_path_buf(),
                force: false,
                json: true,
            }))
            .await,
            Ok(ExitCode::SUCCESS)
        );
        assert_eq!(
            run(BackendCommand::Logs(LogsArguments {
                path: root.path().to_path_buf(),
                tier: None,
                lines: 20,
                json: false,
            }))
            .await,
            Ok(ExitCode::SUCCESS)
        );
        assert_eq!(
            run(BackendCommand::Logs(LogsArguments {
                path: root.path().to_path_buf(),
                tier: Some(BackendTier::Embed),
                lines: 20,
                json: true,
            }))
            .await,
            Ok(ExitCode::FAILURE)
        );
        assert_eq!(
            run(BackendCommand::Cleanup(CleanupArguments {
                path: root.path().to_path_buf(),
                apply: false,
                confirm: None,
                minimum_age_hours: 0,
                maximum_deletions: 8,
                json: true,
            }))
            .await,
            Ok(ExitCode::SUCCESS)
        );
        assert!(!root.path().join(".cartograph/backends").exists());
    }

    struct CleanupFixture {
        root: TempDir,
        log: PathBuf,
        rotated: PathBuf,
        invalid: PathBuf,
        future: PathBuf,
        malicious: PathBuf,
        unsafe_entry: PathBuf,
    }

    fn cleanup_fixture() -> CleanupFixture {
        let root = tempdir().unwrap_or_else(|error| panic!("fixture root failed: {error}"));
        fs::create_dir(root.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("fixture marker failed: {error}"));
        let paths =
            state_paths(root.path()).unwrap_or_else(|error| panic!("state paths failed: {error}"));
        ensure_state_directory(&paths)
            .unwrap_or_else(|error| panic!("state directory failed: {error}"));
        let log = paths.directory.join("llama-0123456789ab.log");
        let oversized =
            File::create(&log).unwrap_or_else(|error| panic!("log fixture create failed: {error}"));
        oversized
            .set_len(MAXIMUM_BACKEND_LOG_BYTES)
            .unwrap_or_else(|error| panic!("log fixture resize failed: {error}"));
        drop(oversized);
        drop(open_log_file(&log).unwrap_or_else(|error| panic!("log rotate failed: {error}")));
        assert_eq!(
            fs::metadata(&log)
                .unwrap_or_else(|error| panic!("current log metadata failed: {error}"))
                .len(),
            0
        );
        let rotated = paths.directory.join("llama-0123456789ab.log.1");
        assert_eq!(
            fs::metadata(&rotated)
                .unwrap_or_else(|error| panic!("rotated log metadata failed: {error}"))
                .len(),
            MAXIMUM_BACKEND_LOG_BYTES
        );
        let invalid = paths.directory.join("llama-fedcba987654.json");
        fs::write(&invalid, b"not-json")
            .unwrap_or_else(|error| panic!("invalid state fixture failed: {error}"));
        let unsafe_entry = paths.directory.join("llama-aaaaaaaaaaaa.log.1");
        fs::create_dir(&unsafe_entry)
            .unwrap_or_else(|error| panic!("unsafe cleanup fixture failed: {error}"));
        let malicious = paths.directory.join("llama-bbbbbbbbbbbb\n\u{1b}[31m.log.1");
        fs::write(&malicious, b"terminal injection fixture")
            .unwrap_or_else(|error| panic!("malicious-name fixture failed: {error}"));
        let future = paths.directory.join("llama-cccccccccccc.json");
        fs::write(&future, br#"{"schemaVersion":3,"futureField":true}"#)
            .unwrap_or_else(|error| panic!("future state fixture failed: {error}"));
        assert!(matches!(
            read_pid_record(&future),
            PidState::UnsupportedVersion
        ));
        CleanupFixture {
            root,
            log,
            rotated,
            invalid,
            future,
            malicious,
            unsafe_entry,
        }
    }

    #[test]
    fn logs_rotate_at_the_bound_and_cleanup_is_dry_run_first() {
        let CleanupFixture {
            root,
            log,
            rotated,
            invalid,
            future,
            malicious,
            unsafe_entry,
        } = cleanup_fixture();
        let dry_run_arguments = CleanupArguments {
            path: root.path().to_path_buf(),
            apply: false,
            confirm: None,
            minimum_age_hours: 0,
            maximum_deletions: 8,
            json: true,
        };
        let report = backend_cleanup_report(&dry_run_arguments)
            .unwrap_or_else(|error| panic!("cleanup report failed: {error}"));
        assert_eq!(report.state_directory, PUBLIC_STATE_DIRECTORY);
        assert!(
            report
                .eligible
                .iter()
                .all(|entry| valid_cleanup_entry_name(&entry.name))
        );
        assert!(report.skipped.iter().any(|skipped| {
            skipped.name.as_deref() == Some("llama-aaaaaaaaaaaa.log.1")
                && skipped.reason == BackendCleanupSkipReason::UNSAFE_ENTRY_TYPE
        }));
        assert!(report.skipped.iter().any(|skipped| {
            skipped.name.is_none() && skipped.reason == BackendCleanupSkipReason::INVALID_ENTRY_NAME
        }));
        assert!(report.skipped.iter().any(|skipped| {
            skipped.name.as_deref() == Some("llama-cccccccccccc.json")
                && skipped.reason == BackendCleanupSkipReason::UNSUPPORTED_STATE_VERSION
        }));
        let encoded = serde_json::to_string(&report)
            .unwrap_or_else(|error| panic!("cleanup report serialization failed: {error}"));
        assert!(!encoded.contains(&root.path().to_string_lossy().into_owned()));
        assert!(!encoded.contains("[31m"));

        let dry_run = run_cleanup(&dry_run_arguments);
        assert_eq!(dry_run, Ok(ExitCode::SUCCESS));
        assert!(rotated.exists());
        assert!(invalid.exists());
        assert!(future.exists());
        assert!(
            run_cleanup(&CleanupArguments {
                path: root.path().to_path_buf(),
                apply: true,
                confirm: Some("wrong".to_owned()),
                minimum_age_hours: 0,
                maximum_deletions: 8,
                json: true,
            })
            .is_err()
        );
        assert_eq!(
            run_cleanup(&CleanupArguments {
                path: root.path().to_path_buf(),
                apply: true,
                confirm: Some(CLEANUP_CONFIRMATION.to_owned()),
                minimum_age_hours: 0,
                maximum_deletions: 8,
                json: true,
            }),
            Ok(ExitCode::SUCCESS)
        );
        assert!(!rotated.exists());
        assert!(!invalid.exists());
        assert!(log.exists());
        assert!(future.exists());
        assert!(malicious.exists());
        assert!(unsafe_entry.is_dir());
    }

    #[test]
    fn state_endpoint_and_argument_boundaries_fail_closed() {
        let root = tempdir().unwrap_or_else(|error| panic!("fixture root failed: {error}"));
        assert!(state_paths(root.path()).is_err());
        fs::create_dir(root.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("fixture marker failed: {error}"));
        let paths =
            state_paths(root.path()).unwrap_or_else(|error| panic!("state paths failed: {error}"));
        ensure_state_directory(&paths)
            .unwrap_or_else(|error| panic!("state directory failed: {error}"));
        assert!(paths.directory.is_dir());
        assert_eq!(
            paths.project,
            fs::canonicalize(root.path()).unwrap_or_default()
        );

        assert!(validate_binary("").is_err());
        assert!(validate_binary("bad\nbinary").is_err());
        assert!(validate_binary(&"b".repeat(4_097)).is_err());
        assert!(validate_binary("llama-server").is_ok());
        assert!(has_parallel_flag(&["--parallel=4".to_owned()]));
        assert!(has_parallel_flag(&["-np".to_owned(), "2".to_owned()]));
        assert!(!has_parallel_flag(&["--cache-ram".to_owned()]));
        for rejected in [
            "--model=other.gguf",
            "--host",
            "--embeddings",
            "--reranking",
        ] {
            assert!(validate_passthrough(&[rejected.to_owned()]).is_err());
        }

        assert_eq!(
            local_endpoint(LOCALHOST_V1_ENDPOINT)
                .unwrap_or_else(|error| panic!("localhost endpoint failed: {error}")),
            Some((LOCALHOST_ENDPOINT.to_owned(), "localhost".to_owned(), 8080,))
        );
        assert_eq!(
            local_endpoint(IPV6_ENDPOINT)
                .unwrap_or_else(|error| panic!("IPv6 endpoint failed: {error}")),
            Some((IPV6_ENDPOINT.to_owned(), "::1".to_owned(), 8081))
        );
        assert!(
            local_endpoint(SECURE_LOCALHOST_ENDPOINT)
                .unwrap_or_else(|error| panic!("HTTPS endpoint failed: {error}"))
                .is_none()
        );
        assert!(
            local_endpoint(REMOTE_ENDPOINT)
                .unwrap_or_else(|error| panic!("remote endpoint failed: {error}"))
                .is_none()
        );
        assert!(local_endpoint("not a URL").is_err());
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("192.0.2.1"));
        assert_eq!(short_hash("stable"), short_hash("stable"));
        assert_ne!(short_hash("stable"), short_hash("different"));
        assert!(valid_backend_id("llama-0123456789ab"));
        assert!(!valid_backend_id("llama-0123456789AG"));
        assert!(!valid_cleanup_entry_name(
            "llama-0123456789ab\n\u{1b}[31m.log.1"
        ));
        assert_eq!(BackendTier::Summarize.label(), "summarize");
        assert_eq!(BackendTier::Local.label(), "local");
        assert_eq!(BackendTier::Ask.label(), "ask");
        assert_eq!(BackendTier::Classify.label(), "classify");
        assert_eq!(BackendTier::Rerank.label(), "rerank");
    }

    struct PidFixture {
        root: TempDir,
        paths: StatePaths,
        log: PathBuf,
        row: BackendRow,
    }

    fn pid_fixture() -> PidFixture {
        let root = tempdir().unwrap_or_else(|error| panic!("fixture root failed: {error}"));
        let marker = root.path().join(".cartograph");
        fs::create_dir(&marker).unwrap_or_else(|error| panic!("fixture marker failed: {error}"));
        let paths =
            state_paths(root.path()).unwrap_or_else(|error| panic!("state paths failed: {error}"));
        ensure_state_directory(&paths)
            .unwrap_or_else(|error| panic!("state directory failed: {error}"));
        let model = root.path().join("fixture.gguf");
        fs::write(&model, b"fixture model")
            .unwrap_or_else(|error| panic!("model fixture failed: {error}"));
        let log = paths.directory.join("fixture.log");
        fs::write(&log, b"first\nsecond\nthird\n")
            .unwrap_or_else(|error| panic!("log fixture failed: {error}"));
        assert_eq!(
            tail_log(&log, 2).unwrap_or_else(|error| panic!("log tail failed: {error}")),
            Some("second\nthird".to_owned())
        );
        assert_eq!(tail_log(&paths.directory.join("missing.log"), 2), Ok(None));
        let unsafe_log = paths.directory.join("unsafe-log");
        fs::create_dir(&unsafe_log)
            .unwrap_or_else(|error| panic!("unsafe log fixture failed: {error}"));
        assert!(tail_log(&unsafe_log, 2).is_err());

        let spec = BackendSpec {
            id: FIXTURE_BACKEND_ID.to_owned(),
            labels: vec!["embed".to_owned()],
            endpoint: UNREACHABLE_ENDPOINT.to_owned(),
            model_path: model.clone(),
            host: "127.0.0.1".to_owned(),
            port: 65_534,
            parallel: 2,
            command: "/usr/bin/false".to_owned(),
            args: vec!["--port".to_owned(), "65534".to_owned()],
            externally_managed: false,
        };
        let row = BackendRow {
            spec: spec.clone(),
            origin: BackendOrigin::Config,
            pid_file_path: paths.directory.join(format!("{FIXTURE_BACKEND_ID}.json")),
            log_path: log.clone(),
            pid_record: None,
            process_health: BackendProcessHealth {
                pid_alive: false,
                endpoint_reachable: false,
            },
            artifact_health: BackendArtifactHealth {
                model_exists: true,
                config_drift: false,
            },
            state: BackendState::Stopped,
            state_error: None,
        };
        PidFixture {
            root,
            paths,
            log,
            row,
        }
    }

    #[tokio::test]
    async fn pid_state_orphan_recovery_and_log_tail_never_trust_unsafe_files() {
        let PidFixture {
            root,
            paths,
            log,
            mut row,
        } = pid_fixture();
        assert!(root.path().join(".cartograph").is_dir());
        assert_eq!(
            write_pid_record(&row, 0),
            Err("child process did not report a valid pid".to_owned())
        );
        write_pid_record(&row, u32::MAX - 1)
            .unwrap_or_else(|error| panic!("pid record write failed: {error}"));
        let record = match read_pid_record(&row.pid_file_path) {
            PidState::Valid(record) => record,
            PidState::Missing | PidState::UnsupportedVersion | PidState::Invalid(_) => {
                panic!("valid pid state was rejected")
            }
        };
        assert!(valid_pid_record(&record));
        assert!(safe_regular_file(&row.pid_file_path));
        assert!(matches!(
            read_pid_record(&paths.directory.join("missing.json")),
            PidState::Missing
        ));

        let orphan = orphan_spec(FIXTURE_BACKEND_ID, &record)
            .unwrap_or_else(|error| panic!("orphan state failed: {error}"));
        assert_eq!(orphan.id, FIXTURE_BACKEND_ID);
        let (orphans, warnings) = discover_orphans(&paths.directory, &BTreeSet::new())
            .unwrap_or_else(|error| panic!("orphan discovery failed: {error}"));
        assert_eq!(orphans.len(), 1);
        assert_eq!(warnings, [] as [String; 0]);

        row.pid_record = Some(record);
        assert_eq!(
            stop_row(&row, false)
                .await
                .unwrap_or_else(|error| panic!("stale pid cleanup failed: {error}")),
            Some(format!("removed stale pid state for {}", u32::MAX - 1))
        );
        assert!(!row.pid_file_path.exists());
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(u32::MAX));
        assert!(wait_for_exit(u32::MAX, Duration::from_millis(1)).await);

        fs::write(paths.directory.join("malformed.json"), b"not-json")
            .unwrap_or_else(|error| panic!("malformed pid fixture failed: {error}"));
        assert!(matches!(
            read_pid_record(&paths.directory.join("malformed.json")),
            PidState::Invalid(message) if message == "state file is malformed"
        ));

        let mut missing_model = row.clone();
        missing_model.artifact_health.model_exists = false;
        missing_model.state = BackendState::MissingModel;
        assert_eq!(
            start_skip_reason(&missing_model).as_deref(),
            Some("configured GGUF model is missing or unsafe")
        );
        missing_model.origin = BackendOrigin::Orphan;
        assert_eq!(
            start_skip_reason(&missing_model).as_deref(),
            Some("orphaned state must be reclaimed with backend stop")
        );
        let status = BackendStatusReport {
            project_path: paths.project,
            state_directory: paths.directory,
            rows: vec![missing_model],
            warnings: vec!["fixture warning".to_owned()],
            unmanaged_reason: None,
        };
        assert!(no_usable_backends(&status));
        render_status(&status, false)
            .unwrap_or_else(|error| panic!("status rendering failed: {error}"));
        let logs = BackendLogsReport {
            status,
            logs: vec![BackendLogEntry {
                labels: vec!["embed".to_owned()],
                path: log,
                exists: true,
                content: "third".to_owned(),
                error: None,
            }],
        };
        render_logs(&logs);
    }
}
