use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use cartograph_agent::ProjectCancellation;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use toml_edit::DocumentMut;

const MAX_DISCOVERY_DEPTH: u8 = 10;
const MAX_DISCOVERY_DIRECTORIES: usize = 50_000;
const MAX_DISCOVERED_PROJECTS: usize = 1_000;
const MAX_HOST_CONFIG_BYTES: u64 = 1024 * 1024;

/// `commandState` of a Cartograph executable resolved through a `PATH` lookup.
const PATH_LOOKUP: &str = "path_lookup";
/// `commandState` of an absolute Cartograph executable when no selected
/// executable is known to compare against.
const ABSOLUTE_UNCHECKED: &str = "absolute_unchecked";
/// `commandState` of an absolute Cartograph executable that resolves to the
/// selected executable.
const CURRENT_ABSOLUTE: &str = "current_absolute";
/// `commandState` of an absolute Cartograph executable that resolves to any
/// other file: the only state that `upgrade --apply` repins.
const STALE_ABSOLUTE: &str = "stale_absolute";
/// `commandState` of a registration whose `command` is another program (for
/// example `/usr/bin/env`, `op run --`, or a custom helper) that launches a
/// Cartograph executable named in its arguments, followed by `serve`.
const WRAPPED: &str = "wrapped";
/// `commandState` of a registration whose `command` is not a Cartograph
/// executable and whose arguments name none either; Cartograph never repins it.
const CUSTOM_COMMAND: &str = "custom_command";
/// File names of the native Cartograph executable.
const CARTOGRAPH_EXECUTABLE_NAMES: [&str; 2] = ["cartograph", "cartograph.exe"];
/// Directory name of the native installation root (`~/.cartograph-cli`).
const INSTALL_ROOT_DIRECTORY: &str = ".cartograph-cli";
/// Installation-root child that holds one directory per installed release.
const RELEASES_DIRECTORY: &str = "versions";
/// Installation-root symlink that points at the active release.
const CURRENT_RELEASE_LINK: &str = "current";
/// Release child that holds the executables.
const EXECUTABLE_DIRECTORY: &str = "bin";
/// Subcommand that follows a wrapped Cartograph executable in its arguments.
const SERVE_SUBCOMMAND: &str = "serve";

const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "coverage",
    "vendor",
    "__tests__",
    "__mocks__",
    "fixtures",
    "fixture",
    "test-beds",
    "test-bed",
];

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(crate) enum HostInspectionError {
    #[error("host inspection options are invalid")]
    InvalidOptions,
    #[error("host inspection root is unavailable")]
    RootUnavailable,
    #[error("host inspection worker failed")]
    WorkerFailed,
    #[error("host inspection was cancelled")]
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveredProject {
    path: String,
    active: bool,
    config_present: bool,
    postgres_configured: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveryReport {
    root: String,
    max_depth: u8,
    directories_visited: usize,
    projects: Vec<DiscoveredProject>,
    truncated: bool,
    stats_scope: &'static str,
}

pub(crate) struct ProjectDiscoveryRequest<'root> {
    active_root: &'root Path,
    requested_root: Option<&'root str>,
    max_depth: u8,
    cancellation: ProjectCancellation,
}

impl<'root> ProjectDiscoveryRequest<'root> {
    pub(crate) const fn new(
        active_root: &'root Path,
        max_depth: u8,
        cancellation: ProjectCancellation,
    ) -> Self {
        Self {
            active_root,
            requested_root: None,
            max_depth,
            cancellation,
        }
    }

    pub(crate) const fn with_requested_root(mut self, requested_root: Option<&'root str>) -> Self {
        self.requested_root = requested_root;
        self
    }
}

struct ProjectDiscoveryWork {
    root: PathBuf,
    active_root: PathBuf,
    max_depth: u8,
    cancellation: ProjectCancellation,
}

struct DiscoveryDirectoryInput<'input> {
    directory: &'input Path,
    depth: u8,
    max_depth: u8,
    active_root: &'input Path,
    projects: &'input mut Vec<DiscoveredProject>,
}

fn discovery_children(
    input: &mut DiscoveryDirectoryInput<'_>,
) -> Result<Vec<PathBuf>, HostInspectionError> {
    let Ok(entries) = fs::read_dir(input.directory) else {
        return Ok(Vec::new());
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if name == ".cartograph" {
            let config = bounded_config(&path.join("config.json"));
            input.projects.push(DiscoveredProject {
                path: path_text(input.directory)?,
                active: input.directory == input.active_root,
                config_present: config.is_some(),
                postgres_configured: config.as_ref().is_some_and(postgres_configured),
            });
            continue;
        }
        if input.depth >= input.max_depth
            || name.starts_with('.')
            || SKIPPED_DIRECTORY_NAMES.contains(&name.as_ref())
        {
            continue;
        }
        children.push(path);
    }
    children.sort();
    children.reverse();
    Ok(children)
}

pub(crate) async fn discover_projects(
    request: ProjectDiscoveryRequest<'_>,
) -> Result<DiscoveryReport, HostInspectionError> {
    let ProjectDiscoveryRequest {
        active_root,
        requested_root,
        max_depth,
        cancellation,
    } = request;
    if max_depth == 0 || max_depth > MAX_DISCOVERY_DEPTH {
        return Err(HostInspectionError::InvalidOptions);
    }
    let root = requested_root.map_or_else(|| active_root.to_path_buf(), PathBuf::from);
    let root = fs::canonicalize(root).map_err(|_| HostInspectionError::RootUnavailable)?;
    if !root.is_dir() {
        return Err(HostInspectionError::RootUnavailable);
    }
    let active_root = active_root.to_path_buf();
    let work = ProjectDiscoveryWork {
        root,
        active_root,
        max_depth,
        cancellation,
    };
    tokio::task::spawn_blocking(move || discover_projects_blocking(work))
        .await
        .map_err(|_| HostInspectionError::WorkerFailed)?
}

fn discover_projects_blocking(
    work: ProjectDiscoveryWork,
) -> Result<DiscoveryReport, HostInspectionError> {
    let ProjectDiscoveryWork {
        root,
        active_root,
        max_depth,
        cancellation,
    } = work;
    let mut stack = vec![(root.clone(), 0_u8)];
    let mut projects = Vec::new();
    let mut visited = 0_usize;
    let mut truncated = false;
    while let Some((directory, depth)) = stack.pop() {
        if cancellation.is_cancelled() {
            return Err(HostInspectionError::Cancelled);
        }
        if visited >= MAX_DISCOVERY_DIRECTORIES || projects.len() >= MAX_DISCOVERED_PROJECTS {
            truncated = true;
            break;
        }
        visited = visited.saturating_add(1);
        let children = discovery_children(&mut DiscoveryDirectoryInput {
            directory: &directory,
            depth,
            max_depth,
            active_root: &active_root,
            projects: &mut projects,
        })?;
        for child in children {
            stack.push((child, depth.saturating_add(1)));
        }
    }
    projects.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(DiscoveryReport {
        root: path_text(&root)?,
        max_depth,
        directories_visited: visited,
        projects,
        truncated,
        stats_scope: "active_project_status_is_returned_separately; sibling_database_connections_are_not_opened",
    })
}

fn bounded_config(path: &Path) -> Option<Value> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_HOST_CONFIG_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn postgres_configured(value: &Value) -> bool {
    matches!(
        value.pointer("/database/provider").and_then(Value::as_str),
        Some("postgres" | "postgresql")
    )
}

fn path_text(path: &Path) -> Result<String, HostInspectionError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or(HostInspectionError::RootUnavailable)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticLocation {
    Global,
    Local,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallTargetDetection {
    pub(crate) target: &'static str,
    pub(crate) location: &'static str,
    pub(crate) config_present: bool,
    pub(crate) config_valid: bool,
    pub(crate) cartograph_configured: bool,
    pub(crate) config_path: &'static str,
    pub(crate) command_state: &'static str,
    /// For a `wrapped` registration, the state of the Cartograph executable
    /// named in its arguments, using the direct-pin vocabulary
    /// (`path_lookup`, `absolute_unchecked`, `current_absolute`, `stale_absolute`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) wrapped_executable_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) managed_database_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repin_command: Option<String>,
    /// Where the configured Cartograph executable sits in the entry. It is not
    /// part of the audit output; a repair uses it to report what changed.
    #[serde(skip)]
    pub(crate) executable: Option<ConfiguredExecutable>,
}

impl InstallTargetDetection {
    /// Whether the registration pins an absolute Cartograph executable other
    /// than the selected one, either as its `command` or inside a wrapper's
    /// arguments.
    pub(crate) fn needs_repin(&self) -> bool {
        self.command_state == STALE_ABSOLUTE
            || self.wrapped_executable_state == Some(STALE_ABSOLUTE)
    }
}

/// The Cartograph executable a registration launches, as written in the host
/// configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfiguredExecutable {
    /// The unchanged `command` of a wrapped registration; `None` when the
    /// executable is the `command` itself.
    pub(crate) wrapper: Option<String>,
    /// Index into `args` of a wrapped executable; `None` for `command`.
    pub(crate) argument_index: Option<usize>,
    /// The configured executable path or `PATH` name.
    pub(crate) path: String,
}

/// The project and home directories whose host configuration is inspected.
#[derive(Clone, Copy)]
pub(crate) struct HostScope<'path> {
    /// Canonical project root; local registrations live under it.
    pub(crate) project_root: &'path Path,
    /// Canonical home directory; global and Claude registrations live under it.
    pub(crate) home: Option<&'path Path>,
    /// Which registration locations to inspect.
    pub(crate) location: DiagnosticLocation,
}

#[derive(Clone, Copy)]
struct HostConfigTarget<'path> {
    target: &'static str,
    location: &'static str,
    path: &'path Path,
    config_path: &'static str,
}

impl<'path> HostConfigTarget<'path> {
    const fn local(target: &'static str, path: &'path Path, config_path: &'static str) -> Self {
        Self {
            target,
            location: "local",
            path,
            config_path,
        }
    }

    const fn global(target: &'static str, path: &'path Path, config_path: &'static str) -> Self {
        Self {
            target,
            location: "global",
            path,
            config_path,
        }
    }
}

pub(crate) fn detect_install_targets(
    project_root: &Path,
    location: DiagnosticLocation,
    selected_executable: Option<&Path>,
) -> Vec<InstallTargetDetection> {
    let home = host_home();
    detect_install_targets_in(
        &HostScope {
            project_root,
            home: home.as_deref(),
            location,
        },
        selected_executable,
    )
}

/// Inspects the Codex, Cursor, and Claude registrations of an explicit scope.
pub(crate) fn detect_install_targets_in(
    scope: &HostScope<'_>,
    selected_executable: Option<&Path>,
) -> Vec<InstallTargetDetection> {
    let HostScope {
        project_root,
        home,
        location,
    } = *scope;
    let mut detections = Vec::new();
    if matches!(
        location,
        DiagnosticLocation::Local | DiagnosticLocation::Both
    ) {
        detections.push(detect_toml(
            &HostConfigTarget::local(
                "codex",
                &project_root.join(".codex/config.toml"),
                ".codex/config.toml",
            ),
            selected_executable,
        ));
        detections.push(detect_json(
            &JsonHostConfigTarget {
                config: HostConfigTarget::local(
                    "cursor",
                    &project_root.join(".cursor/mcp.json"),
                    ".cursor/mcp.json",
                ),
                mode: JsonDetection::TopLevel,
                project_root,
            },
            selected_executable,
        ));
        if let Some(home) = home {
            detections.push(detect_json(
                &JsonHostConfigTarget {
                    config: HostConfigTarget::local(
                        "claude",
                        &home.join(".claude.json"),
                        "~/.claude.json (project entry)",
                    ),
                    mode: JsonDetection::ClaudeProject,
                    project_root,
                },
                selected_executable,
            ));
        }
    }
    if matches!(
        location,
        DiagnosticLocation::Global | DiagnosticLocation::Both
    ) && let Some(home) = home
    {
        detections.push(detect_toml(
            &HostConfigTarget::global(
                "codex",
                &home.join(".codex/config.toml"),
                "~/.codex/config.toml",
            ),
            selected_executable,
        ));
        detections.push(detect_json(
            &JsonHostConfigTarget {
                config: HostConfigTarget::global(
                    "cursor",
                    &home.join(".cursor/mcp.json"),
                    "~/.cursor/mcp.json",
                ),
                mode: JsonDetection::TopLevel,
                project_root,
            },
            selected_executable,
        ));
        detections.push(detect_json(
            &JsonHostConfigTarget {
                config: HostConfigTarget::global(
                    "claude",
                    &home.join(".claude.json"),
                    "~/.claude.json",
                ),
                mode: JsonDetection::TopLevel,
                project_root,
            },
            selected_executable,
        ));
    }
    detections
}

/// The canonical home directory of the process environment, if it exists.
pub(crate) fn host_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok())
        .filter(|path| path.is_dir())
}

fn bounded_text(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_HOST_CONFIG_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn detect_toml(
    config: &HostConfigTarget<'_>,
    selected_executable: Option<&Path>,
) -> InstallTargetDetection {
    let HostConfigTarget {
        target,
        location,
        path,
        config_path,
    } = *config;
    let text = bounded_text(path);
    let parsed = text
        .as_deref()
        .and_then(|text| text.parse::<DocumentMut>().ok());
    let entry = parsed
        .as_ref()
        .and_then(|document| document.get("mcp_servers"))
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|servers| servers.get("cartograph"));
    let fields = entry.and_then(toml_edit::Item::as_table_like);
    let configured_command = fields
        .and_then(|cartograph| cartograph.get("command"))
        .and_then(toml_edit::Item::as_str);
    let configured_args = fields
        .and_then(|cartograph| cartograph.get("args"))
        .and_then(toml_edit::Item::as_array)
        .map(|args| {
            args.iter()
                .map(toml_edit::Value::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    registration_detection(&RegistrationDetectionInput {
        target,
        location,
        config_present: text.is_some(),
        config_valid: parsed.is_some(),
        cartograph_configured: entry.is_some(),
        config_path,
        configured_command,
        configured_args: &configured_args,
        selected_executable,
    })
}

#[derive(Clone, Copy)]
enum JsonDetection {
    TopLevel,
    ClaudeProject,
}

#[derive(Clone, Copy)]
struct JsonHostConfigTarget<'path> {
    config: HostConfigTarget<'path>,
    mode: JsonDetection,
    project_root: &'path Path,
}

fn detect_json(
    config: &JsonHostConfigTarget<'_>,
    selected_executable: Option<&Path>,
) -> InstallTargetDetection {
    let JsonHostConfigTarget {
        config:
            HostConfigTarget {
                target,
                location,
                path,
                config_path,
            },
        mode,
        project_root,
    } = *config;
    let text = bounded_text(path);
    let parsed = text
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let project_key = project_root.to_str();
    let configured_value = parsed.as_ref().and_then(|value| match mode {
        JsonDetection::TopLevel => value.pointer("/mcpServers/cartograph"),
        JsonDetection::ClaudeProject => project_key.and_then(|key| {
            value
                .get("projects")
                .and_then(Value::as_object)
                .and_then(|projects| projects.get(key))
                .and_then(|project| project.pointer("/mcpServers/cartograph"))
        }),
    });
    let entry = configured_value.and_then(Value::as_object);
    let configured_command = entry
        .and_then(|entry| entry.get("command"))
        .and_then(Value::as_str);
    let configured_args = entry
        .and_then(|entry| entry.get("args"))
        .and_then(Value::as_array)
        .map(|args| args.iter().map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    registration_detection(&RegistrationDetectionInput {
        target,
        location,
        config_present: text.is_some(),
        config_valid: parsed.is_some(),
        cartograph_configured: configured_value.is_some(),
        config_path,
        configured_command,
        configured_args: &configured_args,
        selected_executable,
    })
}

struct RegistrationDetectionInput<'input> {
    target: &'static str,
    location: &'static str,
    config_present: bool,
    config_valid: bool,
    cartograph_configured: bool,
    config_path: &'static str,
    configured_command: Option<&'input str>,
    /// Entry arguments by position; `None` marks a non-string argument.
    configured_args: &'input [Option<&'input str>],
    selected_executable: Option<&'input Path>,
}

/// How a configured registration launches Cartograph.
struct RegistrationLaunch {
    state: &'static str,
    wrapped_executable_state: Option<&'static str>,
    executable: Option<ConfiguredExecutable>,
}

impl RegistrationLaunch {
    const fn without_executable(state: &'static str) -> Self {
        Self {
            state,
            wrapped_executable_state: None,
            executable: None,
        }
    }
}

fn registration_detection(input: &RegistrationDetectionInput<'_>) -> InstallTargetDetection {
    let launch = registration_launch(input);
    let managed_database_port =
        managed_database_port_from_args(input.configured_args.iter().filter_map(|arg| *arg));
    let mut detection = InstallTargetDetection {
        target: input.target,
        location: input.location,
        config_present: input.config_present,
        config_valid: input.config_valid,
        cartograph_configured: input.cartograph_configured,
        config_path: input.config_path,
        command_state: launch.state,
        wrapped_executable_state: launch.wrapped_executable_state,
        managed_database_port,
        repin_command: None,
        executable: launch.executable,
    };
    detection.repin_command = detection.needs_repin().then(|| {
        let managed_port = managed_database_port.map_or_else(String::new, |port| {
            format!(" --managed-database-port {port}")
        });
        format!(
            "cartograph install --yes --target {} --location {}{managed_port} --project-path <path>",
            input.target, input.location,
        )
    });
    detection
}

fn managed_database_port_from_args<'arg>(args: impl IntoIterator<Item = &'arg str>) -> Option<u16> {
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        let value = if argument == "--managed-database-port" {
            args.next()
        } else {
            argument.strip_prefix("--managed-database-port=")
        };
        if let Some(port) = value
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|port| *port > 0)
        {
            return Some(port);
        }
    }
    None
}

/// Classifies how the entry launches Cartograph. Only a `command` that is
/// itself a Cartograph executable is a direct pin; any other program whose
/// arguments name a Cartograph executable followed by `serve` is a wrapper,
/// and everything else is a custom command that Cartograph never repins.
fn registration_launch(input: &RegistrationDetectionInput<'_>) -> RegistrationLaunch {
    let Some(command) = input.configured_command else {
        return RegistrationLaunch::without_executable(unlaunched_state(input));
    };
    if is_cartograph_executable(command) {
        return RegistrationLaunch {
            state: executable_state(command, input.selected_executable),
            wrapped_executable_state: None,
            executable: Some(ConfiguredExecutable {
                wrapper: None,
                argument_index: None,
                path: command.to_owned(),
            }),
        };
    }
    let wrapped =
        wrapped_executable_index(input.configured_args.iter().copied()).and_then(|index| {
            input
                .configured_args
                .get(index)
                .copied()
                .flatten()
                .map(|path| (index, path))
        });
    let Some((index, path)) = wrapped else {
        return RegistrationLaunch::without_executable(CUSTOM_COMMAND);
    };
    RegistrationLaunch {
        state: WRAPPED,
        wrapped_executable_state: Some(executable_state(path, input.selected_executable)),
        executable: Some(ConfiguredExecutable {
            wrapper: Some(command.to_owned()),
            argument_index: Some(index),
            path: path.to_owned(),
        }),
    }
}

const fn unlaunched_state(input: &RegistrationDetectionInput<'_>) -> &'static str {
    if !input.config_valid {
        "unavailable"
    } else if !input.cartograph_configured {
        "not_configured"
    } else {
        "missing_command"
    }
}

/// Compares a Cartograph executable path with the selected executable.
fn executable_state(executable: &str, selected_executable: Option<&Path>) -> &'static str {
    let configured = Path::new(executable);
    if !configured.is_absolute() {
        return PATH_LOOKUP;
    }
    let Some(selected) = selected_executable else {
        return ABSOLUTE_UNCHECKED;
    };
    let configured = fs::canonicalize(configured).unwrap_or_else(|_| configured.to_path_buf());
    let selected = fs::canonicalize(selected).unwrap_or_else(|_| selected.to_path_buf());
    if configured == selected {
        CURRENT_ABSOLUTE
    } else {
        STALE_ABSOLUTE
    }
}

/// Whether `command` names a native Cartograph executable: its file name is
/// `cartograph` or `cartograph.exe`, it sits in a native installation's
/// `versions/<release>/bin/` or `current/bin/` directory, or it is an absolute
/// path (such as a symlink) that resolves to one of those.
pub(crate) fn is_cartograph_executable(command: &str) -> bool {
    let path = Path::new(command);
    names_cartograph_executable(path)
        || (path.is_absolute()
            && fs::canonicalize(path).is_ok_and(|resolved| names_cartograph_executable(&resolved)))
}

fn names_cartograph_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| CARTOGRAPH_EXECUTABLE_NAMES.contains(&name))
        || in_native_install_bin(path)
}

fn in_native_install_bin(path: &Path) -> bool {
    let Some(release) = path
        .parent()
        .filter(|bin| bin.file_name() == Some(OsStr::new(EXECUTABLE_DIRECTORY)))
        .and_then(Path::parent)
    else {
        return false;
    };
    let install_root = if release.file_name() == Some(OsStr::new(CURRENT_RELEASE_LINK)) {
        release.parent()
    } else {
        release
            .parent()
            .filter(|releases| releases.file_name() == Some(OsStr::new(RELEASES_DIRECTORY)))
            .and_then(Path::parent)
    };
    install_root.and_then(Path::file_name) == Some(OsStr::new(INSTALL_ROOT_DIRECTORY))
}

/// Position of the Cartograph executable that a wrapper launches: the first
/// argument naming a Cartograph executable that is immediately followed by
/// `serve`. `None` items stand for non-string arguments and never match, so
/// the index addresses the original argument array.
pub(crate) fn wrapped_executable_index<'arg>(
    args: impl IntoIterator<Item = Option<&'arg str>>,
) -> Option<usize> {
    let args = args.into_iter().collect::<Vec<_>>();
    args.array_windows().position(|&[executable, subcommand]| {
        subcommand == Some(SERVE_SUBCOMMAND) && executable.is_some_and(is_cartograph_executable)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_config_detection_never_needs_a_database_secret() {
        assert!(postgres_configured(&serde_json::json!({
            "database": {"provider": "postgres"}
        })));
        assert!(!postgres_configured(&serde_json::json!({})));
    }

    #[test]
    fn discovery_is_bounded_and_skips_fixture_projects() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let project = root.path().join("apps/real");
        let fixture = root.path().join("fixtures/not-real");
        fs::create_dir_all(project.join(".cartograph"))
            .unwrap_or_else(|error| panic!("project marker failed: {error}"));
        fs::create_dir_all(fixture.join(".cartograph"))
            .unwrap_or_else(|error| panic!("fixture marker failed: {error}"));
        let report = discover_projects_blocking(ProjectDiscoveryWork {
            root: root.path().to_path_buf(),
            active_root: project.clone(),
            max_depth: 4,
            cancellation: ProjectCancellation::new(),
        })
        .unwrap_or_else(|error| panic!("discovery failed: {error}"));
        assert_eq!(report.projects.len(), 1);
        assert_eq!(report.projects[0].path, project.to_string_lossy());
        assert!(report.projects[0].active);
        assert!(!report.truncated);
    }

    #[tokio::test]
    async fn async_discovery_rejects_invalid_roots_depths_and_cancellation() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        assert_eq!(
            discover_projects(ProjectDiscoveryRequest::new(
                root.path(),
                0,
                ProjectCancellation::new(),
            ))
            .await,
            Err(HostInspectionError::InvalidOptions)
        );
        assert_eq!(
            discover_projects(
                ProjectDiscoveryRequest::new(root.path(), 1, ProjectCancellation::new())
                    .with_requested_root(Some("missing-host-root")),
            )
            .await,
            Err(HostInspectionError::RootUnavailable)
        );
        let cancellation = ProjectCancellation::new();
        cancellation.cancel();
        assert_eq!(
            discover_projects(ProjectDiscoveryRequest::new(root.path(), 1, cancellation)).await,
            Err(HostInspectionError::Cancelled)
        );

        fs::create_dir_all(root.path().join("project/.cartograph"))
            .unwrap_or_else(|error| panic!("project marker failed: {error}"));
        fs::write(
            root.path().join("project/.cartograph/config.json"),
            br#"{"database":{"provider":"postgresql"}}"#,
        )
        .unwrap_or_else(|error| panic!("project config failed: {error}"));
        let report = discover_projects(ProjectDiscoveryRequest::new(
            root.path(),
            MAX_DISCOVERY_DEPTH,
            ProjectCancellation::new(),
        ))
        .await
        .unwrap_or_else(|error| panic!("async discovery failed: {error}"));
        assert_eq!(report.projects.len(), 1);
        assert!(report.projects[0].config_present);
        assert!(report.projects[0].postgres_configured);
        assert_eq!(
            report.stats_scope,
            "active_project_status_is_returned_separately; sibling_database_connections_are_not_opened"
        );
    }

    #[test]
    fn host_config_detection_distinguishes_missing_invalid_and_configured_targets() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let codex = root.path().join("config.toml");
        let missing = detect_toml(
            &HostConfigTarget::local("codex", &codex, ".codex/config.toml"),
            None,
        );
        assert!(!missing.config_present);
        assert!(!missing.config_valid);
        assert!(!missing.cartograph_configured);

        fs::write(&codex, "not = [valid")
            .unwrap_or_else(|error| panic!("invalid TOML fixture failed: {error}"));
        let invalid = detect_toml(
            &HostConfigTarget::local("codex", &codex, ".codex/config.toml"),
            None,
        );
        assert!(invalid.config_present);
        assert!(!invalid.config_valid);
        assert!(!invalid.cartograph_configured);

        fs::write(
            &codex,
            "[mcp_servers.cartograph]\ncommand = '/usr/local/bin/cartograph'\nargs = ['serve', '--mcp', '--managed-database-port', '55435']\n",
        )
        .unwrap_or_else(|error| panic!("valid TOML fixture failed: {error}"));
        let configured = detect_toml(
            &HostConfigTarget::local("codex", &codex, ".codex/config.toml"),
            Some(&codex),
        );
        assert!(configured.config_valid);
        assert!(configured.cartograph_configured);
        assert_eq!(configured.command_state, "stale_absolute");
        assert_eq!(configured.managed_database_port, Some(55_435));
        assert!(configured.repin_command.is_some());
        assert!(bounded_text(&codex).is_some());

        let cursor = root.path().join("mcp.json");
        fs::write(
            &cursor,
            br#"{"mcpServers":{"cartograph":{"command":"cartograph","args":["serve","--mcp","--managed-database-port=55436"]}}}"#,
        )
        .unwrap_or_else(|error| panic!("cursor fixture failed: {error}"));
        let cursor_detection = detect_json(
            &JsonHostConfigTarget {
                config: HostConfigTarget::local("cursor", &cursor, ".cursor/mcp.json"),
                mode: JsonDetection::TopLevel,
                project_root: root.path(),
            },
            None,
        );
        assert!(cursor_detection.config_present);
        assert!(cursor_detection.config_valid);
        assert!(cursor_detection.cartograph_configured);
        assert_eq!(cursor_detection.managed_database_port, Some(55_436));

        let claude = root.path().join("claude.json");
        let project_key = root.path().to_string_lossy().into_owned();
        fs::write(
            &claude,
            serde_json::to_vec(&serde_json::json!({
                "projects": {
                    (project_key): {
                        "mcpServers": {"cartograph": {"command": "cartograph"}}
                    }
                }
            }))
            .unwrap_or_else(|error| panic!("Claude fixture encode failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("Claude fixture failed: {error}"));
        let claude_detection = detect_json(
            &JsonHostConfigTarget {
                config: HostConfigTarget::local(
                    "claude",
                    &claude,
                    "~/.claude.json (project entry)",
                ),
                mode: JsonDetection::ClaudeProject,
                project_root: root.path(),
            },
            None,
        );
        assert!(claude_detection.config_valid);
        assert!(claude_detection.cartograph_configured);

        fs::create_dir(root.path().join("not-a-config"))
            .unwrap_or_else(|error| panic!("unsafe config fixture failed: {error}"));
        assert!(bounded_text(&root.path().join("not-a-config")).is_none());
        assert!(bounded_config(&root.path().join("not-a-config")).is_none());
        assert!(host_home().is_some());
    }

    fn cursor_detection(config: &Path, entry: &Value, selected: &Path) -> InstallTargetDetection {
        fs::write(
            config,
            serde_json::to_vec(&serde_json::json!({"mcpServers": {"cartograph": entry}}))
                .unwrap_or_else(|error| panic!("cursor fixture encode failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("cursor fixture failed: {error}"));
        detect_json(
            &JsonHostConfigTarget {
                config: HostConfigTarget::local("cursor", config, ".cursor/mcp.json"),
                mode: JsonDetection::TopLevel,
                project_root: config.parent().unwrap_or(config),
            },
            Some(selected),
        )
    }

    #[test]
    fn wrapped_registrations_are_never_classified_as_stale_direct_pins() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let selected = root.path().join("cartograph");
        fs::write(&selected, b"fixture")
            .unwrap_or_else(|error| panic!("selected executable failed: {error}"));
        let stale = root
            .path()
            .join(".cartograph-cli/versions/v2.1.30/bin/cartograph")
            .to_string_lossy()
            .into_owned();
        let config = root.path().join("mcp.json");

        let wrapped = cursor_detection(
            &config,
            &serde_json::json!({
                "command": "/usr/bin/env",
                "args": [stale, "serve", "--mcp", "--managed-database-port", "55437"],
                "env": {"EXAMPLE_FLAG": "fixture-env-value"},
                "cwd": "/srv/work"
            }),
            &selected,
        );
        assert_eq!(wrapped.command_state, "wrapped");
        assert_eq!(wrapped.wrapped_executable_state, Some("stale_absolute"));
        assert!(wrapped.needs_repin());
        assert_eq!(wrapped.managed_database_port, Some(55_437));
        assert_eq!(
            wrapped.executable,
            Some(ConfiguredExecutable {
                wrapper: Some("/usr/bin/env".to_owned()),
                argument_index: Some(0),
                path: stale.clone(),
            })
        );
        let encoded = serde_json::to_string(&wrapped)
            .unwrap_or_else(|error| panic!("detection encode failed: {error}"));
        assert!(encoded.contains(r#""commandState":"wrapped""#));
        assert!(encoded.contains(r#""wrappedExecutableState":"stale_absolute""#));
        assert!(!encoded.contains("fixture-env-value"));
        assert!(!encoded.contains(&stale));

        let current = cursor_detection(
            &config,
            &serde_json::json!({
                "command": "/usr/bin/env",
                "args": [selected.to_string_lossy(), "serve", "--mcp"]
            }),
            &selected,
        );
        assert_eq!(current.command_state, "wrapped");
        assert_eq!(current.wrapped_executable_state, Some("current_absolute"));
        assert!(!current.needs_repin());
        assert!(current.repin_command.is_none());

        let path_lookup = cursor_detection(
            &config,
            &serde_json::json!({"command": "direnv", "args": ["exec", ".", "cartograph", "serve"]}),
            &selected,
        );
        assert_eq!(path_lookup.wrapped_executable_state, Some("path_lookup"));
        assert!(!path_lookup.needs_repin());

        let custom = cursor_detection(
            &config,
            &serde_json::json!({"command": "/opt/helpers/start-mcp.sh", "args": ["--mcp"]}),
            &selected,
        );
        assert_eq!(custom.command_state, "custom_command");
        assert_eq!(custom.wrapped_executable_state, None);
        assert!(!custom.needs_repin());
        assert!(custom.repin_command.is_none());

        let direct = cursor_detection(
            &config,
            &serde_json::json!({"command": stale, "args": ["serve", "--mcp"]}),
            &selected,
        );
        assert_eq!(direct.command_state, "stale_absolute");
        assert!(direct.needs_repin());
        assert!(direct.repin_command.is_some());
    }

    #[test]
    fn codex_wrapper_arguments_locate_the_embedded_executable_by_position() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let codex = root.path().join("config.toml");
        fs::write(
            &codex,
            "[mcp_servers]\ncartograph = { command = \"op\", args = [\"run\", \"--\", \"/opt/old/cartograph\", \"serve\", \"--mcp\"], env = { EXAMPLE_FLAG = \"1\" } }\n",
        )
        .unwrap_or_else(|error| panic!("codex fixture failed: {error}"));
        let detection = detect_toml(
            &HostConfigTarget::local("codex", &codex, ".codex/config.toml"),
            Some(&codex),
        );
        assert!(detection.cartograph_configured);
        assert_eq!(detection.command_state, "wrapped");
        assert_eq!(detection.wrapped_executable_state, Some("stale_absolute"));
        assert_eq!(
            detection
                .executable
                .as_ref()
                .and_then(|executable| executable.argument_index),
            Some(2)
        );
    }

    #[test]
    fn cartograph_executables_are_recognized_by_name_layout_or_resolution() {
        for executable in [
            "cartograph",
            "/usr/local/bin/cartograph",
            "C:/tools/cartograph.exe",
            "/home/dev/.cartograph-cli/versions/v2.1.30/bin/cg",
            "/home/dev/.cartograph-cli/current/bin/cg",
        ] {
            assert!(is_cartograph_executable(executable), "{executable}");
        }
        for other in [
            "op",
            "/usr/bin/env",
            "/opt/sdk/current/bin/java",
            "/opt/sdk/versions/v1/bin/java",
            "/home/dev/.cartograph-cli/versions/v2.1.30/lib/cg",
        ] {
            assert!(!is_cartograph_executable(other), "{other}");
        }
        assert_eq!(
            wrapped_executable_index([Some("run"), None, Some("cartograph"), Some("serve")]),
            Some(2)
        );
        assert_eq!(
            wrapped_executable_index([Some("cartograph"), Some("--mcp"), Some("serve")]),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_resolves_to_a_cartograph_executable_is_a_direct_pin() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let installed = root.path().join("tools/cartograph");
        fs::create_dir_all(root.path().join("tools"))
            .unwrap_or_else(|error| panic!("tools fixture failed: {error}"));
        fs::write(&installed, b"fixture")
            .unwrap_or_else(|error| panic!("installed fixture failed: {error}"));
        let alias = root.path().join("cg");
        symlink(&installed, &alias).unwrap_or_else(|error| panic!("alias failed: {error}"));
        let helper = root.path().join("start-mcp");
        fs::write(&helper, b"fixture").unwrap_or_else(|error| panic!("helper failed: {error}"));
        assert!(is_cartograph_executable(&alias.to_string_lossy()));
        assert!(!is_cartograph_executable(&helper.to_string_lossy()));
    }
}
