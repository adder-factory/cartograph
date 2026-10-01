mod jsonc;
mod registration;

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, Table, value};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 4096;
const PRIVATE_CONFIG_MODE: u32 = 0o600;
const GUIDANCE_MODE: u32 = 0o644;
const IGNORE_FILE_MODE: u32 = 0o644;
const IGNORE_COMMENT: &str = "# Cartograph private agent configuration";
const SECTION_START: &str = "<!-- CARTOGRAPH_START -->";
const SECTION_END: &str = "<!-- CARTOGRAPH_END -->";
const OPENCODE_SCHEMA: &str = "https://opencode.ai/config.json";
const SKILL_TEMPLATE: &str = include_str!("../assets/cartograph-skill.md");
const PERMISSIONS: [&str; 7] = [
    "mcp__cartograph__cartograph_find",
    "mcp__cartograph__cartograph_context",
    "mcp__cartograph__cartograph_graph",
    "mcp__cartograph__cartograph_node",
    "mcp__cartograph__cartograph_files",
    "mcp__cartograph__cartograph_at_range",
    "mcp__cartograph__cartograph_status",
];
const INSTRUCTIONS: &str = r"<!-- CARTOGRAPH_START -->
## Cartograph

Cartograph v2 is this workspace's native Rust code-intelligence MCP server. Its
graph is stored in PostgreSQL 18 with ParadeDB and pgvector; SQLite is not a
supported runtime or fallback.

When Cartograph is initialized, call `cartograph_status` before trusting graph
evidence. Start coding tasks with `cartograph_context`, narrow with
`cartograph_find`, `cartograph_node`, and `cartograph_graph`, then use
`cartograph_review` and `cartograph_affected` after edits. Keep stale or unknown
freshness explicit, and run the repository's real quality gates before claiming
completion. If MCP transport is unavailable, say so and use the native CLI as a
control path; a CLI result does not prove an already-open host restarted MCP.

If Cartograph is not initialized, ask before running `cartograph db start`,
`cartograph doctor`, and `cartograph index`. Never print or commit a PostgreSQL
URL. Use the narrowest MCP profile that supports the task.
<!-- CARTOGRAPH_END -->";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[repr(usize)]
pub(crate) enum InstallTarget {
    Claude,
    Cursor,
    Codex,
    Codebuddy,
    Copilot,
    Codewhale,
    Zed,
    Opencode,
    Hermes,
    Gemini,
    Antigravity,
    Kiro,
    Factory,
    Rovo,
    Qoder,
    Bob,
    Kimi,
    Pi,
    Reasonix,
}

impl InstallTarget {
    const LABELS: [&'static str; 19] = [
        "claude",
        "cursor",
        "codex",
        "codebuddy",
        "copilot",
        "codewhale",
        "zed",
        "opencode",
        "hermes",
        "gemini",
        "antigravity",
        "kiro",
        "factory",
        "rovo",
        "qoder",
        "bob",
        "kimi",
        "pi",
        "reasonix",
    ];

    pub(crate) const ALL: [Self; 19] = [
        Self::Claude,
        Self::Cursor,
        Self::Codex,
        Self::Codebuddy,
        Self::Copilot,
        Self::Codewhale,
        Self::Zed,
        Self::Opencode,
        Self::Hermes,
        Self::Gemini,
        Self::Antigravity,
        Self::Kiro,
        Self::Factory,
        Self::Rovo,
        Self::Qoder,
        Self::Bob,
        Self::Kimi,
        Self::Pi,
        Self::Reasonix,
    ];

    pub(crate) const fn label(self) -> &'static str {
        Self::LABELS[self as usize]
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.label() == value.trim().to_ascii_lowercase())
    }

    pub(crate) const fn supports(self, location: InstallLocation) -> bool {
        !matches!(location, InstallLocation::Local)
            || !matches!(self, Self::Hermes | Self::Antigravity | Self::Reasonix)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum InstallLocation {
    #[default]
    Global,
    Local,
}

#[derive(Clone, Copy)]
enum StaticConfigRoot {
    Home,
    Project,
}

#[derive(Clone, Copy)]
struct StaticConfigRule {
    target: InstallTarget,
    location: InstallLocation,
    root: StaticConfigRoot,
    suffix: &'static str,
}

macro_rules! static_config {
    ($target:expr, $location:expr, $root:expr, $suffix:expr $(,)?) => {
        StaticConfigRule {
            target: $target,
            location: $location,
            root: $root,
            suffix: $suffix,
        }
    };
}

const STATIC_CONFIG_RULES: [StaticConfigRule; 24] = [
    static_config!(
        InstallTarget::Claude,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".claude.json",
    ),
    static_config!(
        InstallTarget::Claude,
        InstallLocation::Local,
        StaticConfigRoot::Home,
        ".claude.json",
    ),
    static_config!(
        InstallTarget::Cursor,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".cursor/mcp.json",
    ),
    static_config!(
        InstallTarget::Cursor,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".cursor/mcp.json",
    ),
    static_config!(
        InstallTarget::Codex,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".codex/config.toml",
    ),
    static_config!(
        InstallTarget::Codex,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".codex/config.toml",
    ),
    static_config!(
        InstallTarget::Codewhale,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".codewhale/mcp.json",
    ),
    static_config!(
        InstallTarget::Codewhale,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".codewhale/mcp.json",
    ),
    static_config!(
        InstallTarget::Zed,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".zed/settings.json",
    ),
    static_config!(
        InstallTarget::Opencode,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        "opencode.json",
    ),
    static_config!(
        InstallTarget::Gemini,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".gemini/settings.json",
    ),
    static_config!(
        InstallTarget::Gemini,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".gemini/settings.json",
    ),
    static_config!(
        InstallTarget::Kiro,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".kiro/settings/mcp.json",
    ),
    static_config!(
        InstallTarget::Kiro,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".kiro/settings/mcp.json",
    ),
    static_config!(
        InstallTarget::Factory,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".factory/mcp.json",
    ),
    static_config!(
        InstallTarget::Factory,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".factory/mcp.json",
    ),
    static_config!(
        InstallTarget::Qoder,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".qoder/settings.json",
    ),
    static_config!(
        InstallTarget::Qoder,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".qoder/settings.local.json",
    ),
    static_config!(
        InstallTarget::Bob,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".bob/mcp_settings.json",
    ),
    static_config!(
        InstallTarget::Bob,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".bob/mcp.json",
    ),
    static_config!(
        InstallTarget::Kimi,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".kimi-code/mcp.json",
    ),
    static_config!(
        InstallTarget::Pi,
        InstallLocation::Local,
        StaticConfigRoot::Project,
        ".pi/mcp.json",
    ),
    static_config!(
        InstallTarget::Reasonix,
        InstallLocation::Global,
        StaticConfigRoot::Home,
        ".reasonix/config.json",
    ),
    static_config!(
        InstallTarget::Reasonix,
        InstallLocation::Local,
        StaticConfigRoot::Home,
        ".reasonix/config.json",
    ),
];

pub(crate) struct InstallRequestInput<'path> {
    pub(crate) project_root: &'path Path,
    pub(crate) executable: &'path Path,
    pub(crate) target: InstallTarget,
    pub(crate) location: InstallLocation,
    pub(crate) command_override: Option<&'path str>,
    pub(crate) permissions: bool,
}

pub(crate) struct InstallRequest {
    project_root: PathBuf,
    executable: String,
    target: InstallTarget,
    location: InstallLocation,
    home: PathBuf,
    command_override: Option<String>,
    managed_database_port: Option<u16>,
    permissions: bool,
    use_environment: bool,
}

impl InstallRequest {
    pub(crate) fn new(input: &InstallRequestInput<'_>) -> Result<Self, InstallError> {
        let &InstallRequestInput {
            project_root,
            executable,
            target,
            location,
            command_override,
            permissions,
        } = input;
        let project_root = canonical_directory(project_root, InstallError::ProjectRoot)?;
        let executable_path = executable
            .canonicalize()
            .map_err(|_| InstallError::Executable)?;
        if !executable_path.is_file() {
            return Err(InstallError::Executable);
        }
        let registration_executable =
            stable_registration_executable(&executable_path).unwrap_or(executable_path);
        let executable = path_text(&registration_executable)?.to_owned();
        let home = host_home()?;
        let command_override = command_override
            .map(validate_command)
            .transpose()?
            .map(str::to_owned);
        Ok(Self {
            project_root,
            executable,
            target,
            location,
            home,
            command_override,
            managed_database_port: None,
            permissions,
            use_environment: true,
        })
    }

    #[cfg(test)]
    fn new_for_test(input: &TestInstallRequest<'_>) -> Result<Self, InstallError> {
        let mut request = Self::new(&InstallRequestInput {
            project_root: input.project_root,
            executable: input.executable,
            target: input.target,
            location: input.location,
            command_override: input.command_override,
            permissions: input.permissions,
        })?
        .with_home(input.home)?;
        request.use_environment = false;
        Ok(request)
    }

    /// Resolves host configuration under `home` instead of the process
    /// environment's home directory.
    pub(crate) fn with_home(mut self, home: &Path) -> Result<Self, InstallError> {
        self.home = canonical_directory(home, InstallError::HostHome)?;
        Ok(self)
    }

    pub(crate) fn detected(&self) -> bool {
        let Ok(config) = self.config_location() else {
            return false;
        };
        config.path.exists() || config.path.parent().is_some_and(std::path::Path::exists)
    }

    pub(crate) const fn with_managed_database_port(mut self, port: u16) -> Self {
        self.managed_database_port = Some(port);
        self
    }

    /// The command written into the registration: the `--command` override,
    /// or the stable launcher of the absolute native executable.
    pub(crate) fn command(&self) -> &str {
        self.command_override.as_deref().unwrap_or(&self.executable)
    }

    fn local(&self) -> bool {
        self.location == InstallLocation::Local
    }

    fn server_args(&self) -> Result<Vec<String>, InstallError> {
        let mut args = vec!["serve".to_owned(), "--mcp".to_owned()];
        if self.local() {
            args.push("--project-path".to_owned());
            args.push(path_text(&self.project_root)?.to_owned());
            if let Some(port) = self.managed_database_port {
                args.push("--managed-database-port".to_owned());
                args.push(port.to_string());
            }
        }
        Ok(args)
    }

    fn config_location(&self) -> Result<ConfigLocation, InstallError> {
        if let Some(location) = self.static_config_location() {
            return Ok(location);
        }
        match self.target {
            InstallTarget::Codebuddy => Ok(self.codebuddy_location()),
            InstallTarget::Copilot if self.local() => Ok(self.copilot_local_location()),
            InstallTarget::Copilot => Ok(self.dynamic_config_location(
                &self.env_root("COPILOT_HOME", ".copilot")?,
                "mcp-config.json",
            )),
            InstallTarget::Zed => {
                Ok(self.dynamic_config_location(&self.xdg_root()?, "zed/settings.json"))
            }
            InstallTarget::Opencode => {
                Ok(self.dynamic_config_location(&self.xdg_root()?, "opencode/opencode.json"))
            }
            InstallTarget::Hermes => Ok(self
                .dynamic_config_location(&self.env_root("HERMES_HOME", ".hermes")?, "config.yaml")),
            InstallTarget::Antigravity => Ok(self.antigravity_location()),
            InstallTarget::Rovo => self.rovo_location(),
            InstallTarget::Kimi => Ok(self.dynamic_config_location(
                &self.env_root("KIMI_CODE_HOME", ".kimi-code")?,
                "mcp.json",
            )),
            InstallTarget::Pi => Ok(self.dynamic_config_location(
                &self.env_root("PI_CODING_AGENT_DIR", ".pi/agent")?,
                "mcp.json",
            )),
            _ => Err(InstallError::InvalidConfig),
        }
    }

    fn static_config_location(&self) -> Option<ConfigLocation> {
        let rule = STATIC_CONFIG_RULES
            .iter()
            .find(|rule| rule.target == self.target && rule.location == self.location)?;
        let root = match rule.root {
            StaticConfigRoot::Home => &self.home,
            StaticConfigRoot::Project => &self.project_root,
        };
        Some(config_under(root, rule.suffix))
    }

    fn dynamic_config_location(&self, root: &Path, suffix: &str) -> ConfigLocation {
        let allowed_root = if root.starts_with(&self.home) {
            self.home.clone()
        } else if root.starts_with(&self.project_root) {
            self.project_root.clone()
        } else {
            root.to_path_buf()
        };
        ConfigLocation {
            path: root.join(suffix),
            allowed_root,
        }
    }

    fn copilot_local_location(&self) -> ConfigLocation {
        let direct = self.project_root.join(".mcp.json");
        let github = self.project_root.join(".github/mcp.json");
        ConfigLocation {
            path: if github.exists() && !direct.exists() {
                github
            } else {
                direct
            },
            allowed_root: self.project_root.clone(),
        }
    }

    fn env_root(&self, name: &str, fallback: &str) -> Result<PathBuf, InstallError> {
        if !self.use_environment {
            return Ok(self.home.join(fallback));
        }
        env_root(name, &self.home, fallback)
    }

    fn xdg_root(&self) -> Result<PathBuf, InstallError> {
        self.env_root("XDG_CONFIG_HOME", ".config")
    }

    fn codebuddy_location(&self) -> ConfigLocation {
        let candidates = if self.local() {
            vec![
                self.project_root.join(".mcp.json"),
                self.project_root.join("mcp.json"),
            ]
        } else {
            vec![
                self.home.join(".codebuddy/.mcp.json"),
                self.home.join(".codebuddy/mcp.json"),
                self.home.join(".codebuddy.json"),
            ]
        };
        ConfigLocation {
            path: candidates
                .iter()
                .find(|candidate| candidate.exists())
                .cloned()
                .unwrap_or_else(|| candidates[0].clone()),
            allowed_root: if self.local() {
                self.project_root.clone()
            } else {
                self.home.clone()
            },
        }
    }

    fn antigravity_location(&self) -> ConfigLocation {
        let unified = self.home.join(".gemini/config/mcp_config.json");
        let marker = self.home.join(".gemini/config/.migrated");
        let legacy = self.home.join(".gemini/antigravity/mcp_config.json");
        ConfigLocation {
            path: if marker.exists() || unified.exists() {
                unified
            } else {
                legacy
            },
            allowed_root: self.home.clone(),
        }
    }

    fn rovo_location(&self) -> Result<ConfigLocation, InstallError> {
        let root = if self.local() {
            self.project_root.join(".rovodev")
        } else {
            self.home.join(".rovodev")
        };
        let default = ConfigLocation {
            path: root.join("mcp.json"),
            allowed_root: if self.local() {
                self.project_root.clone()
            } else {
                self.home.clone()
            },
        };
        let config = root.join("config.yml");
        let Some(contents) = read_optional_config(&config)? else {
            return Ok(default);
        };
        let Some(raw) = contents.lines().find_map(|line| {
            let line = line.trim();
            (!line.starts_with('#'))
                .then(|| line.strip_prefix("mcpConfigPath:").map(str::trim))
                .flatten()
                .filter(|value| !value.is_empty())
        }) else {
            return Ok(default);
        };
        let raw = strip_yaml_scalar(raw);
        let candidate = if raw == "~" {
            self.home.clone()
        } else if let Some(suffix) = raw.strip_prefix("~/") {
            self.home.join(suffix)
        } else {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                self.project_root.join(path)
            }
        };
        let Some(parent) = candidate.parent() else {
            return Ok(default);
        };
        let Ok(allowed_root) = parent.canonicalize() else {
            return Ok(default);
        };
        if !allowed_root.is_dir() {
            return Ok(default);
        }
        Ok(ConfigLocation {
            path: candidate,
            allowed_root,
        })
    }
}

/// The command `install` registers for `executable` without a `--command`
/// override: the stable `current/bin` launcher of a versioned native
/// installation, otherwise the canonical executable itself.
pub(crate) fn registration_executable(executable: &Path) -> Option<String> {
    let canonical = executable.canonicalize().ok()?;
    let registered = stable_registration_executable(&canonical).unwrap_or(canonical);
    registered.to_str().map(str::to_owned)
}

fn stable_registration_executable(executable: &Path) -> Option<PathBuf> {
    let file_name = executable.file_name()?;
    let bin = executable.parent()?;
    let release = bin.parent()?;
    let versions = release.parent()?;
    let install_root = versions.parent()?;
    if bin.file_name()? != "bin"
        || versions.file_name()? != "versions"
        || !release
            .file_name()?
            .to_str()?
            .strip_prefix('v')?
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
    {
        return None;
    }
    let current = install_root.join("current");
    if !fs::symlink_metadata(&current)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let candidate = current.join("bin").join(file_name);
    (candidate.canonicalize().ok().as_deref() == Some(executable)).then_some(candidate)
}

#[cfg(test)]
struct TestInstallRequest<'path> {
    project_root: &'path Path,
    executable: &'path Path,
    target: InstallTarget,
    location: InstallLocation,
    home: &'path Path,
    command_override: Option<&'path str>,
    permissions: bool,
}

#[derive(Clone)]
struct ConfigLocation {
    path: PathBuf,
    allowed_root: PathBuf,
}

fn config_under(root: &Path, suffix: &str) -> ConfigLocation {
    ConfigLocation {
        path: root.join(suffix),
        allowed_root: root.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum InstallAction {
    Created,
    Updated,
    Unchanged,
    Removed,
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallFileReport {
    path: PathBuf,
    action: InstallAction,
}

impl InstallFileReport {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) const fn action(&self) -> InstallAction {
        self.action
    }
}

impl InstallAction {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::Removed => "removed",
            Self::NotFound => "not-found",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallReport {
    target: InstallTarget,
    location: InstallLocation,
    executable: String,
    project_root: PathBuf,
    files: Vec<InstallFileReport>,
}

impl InstallReport {
    pub(crate) fn changed(&self) -> bool {
        self.files.iter().any(|file| {
            matches!(
                file.action,
                InstallAction::Created | InstallAction::Updated | InstallAction::Removed
            )
        })
    }

    pub(crate) const fn target(&self) -> InstallTarget {
        self.target
    }

    pub(crate) const fn location(&self) -> InstallLocation {
        self.location
    }

    pub(crate) fn executable(&self) -> &str {
        &self.executable
    }

    pub(crate) fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub(crate) fn files(&self) -> &[InstallFileReport] {
        &self.files
    }
}

pub(crate) fn install(request: &InstallRequest) -> Result<InstallReport, InstallError> {
    let mut report = install_registration(request)?;
    report.files.extend(install_artifacts(request)?);
    if request.local() {
        ensure_project_config_ignored(request, &report.files)?;
    }
    Ok(report)
}

/// Writes only the MCP registration entry, without guidance artifacts.
///
/// An existing entry is merged rather than replaced: keys and extra server
/// arguments Cartograph does not own survive, and a wrapper registration keeps
/// its command and arguments except for its absolute Cartograph executable.
pub(crate) fn install_registration(
    request: &InstallRequest,
) -> Result<InstallReport, InstallError> {
    if !request.target.supports(request.location) {
        return Err(InstallError::UnsupportedLocation {
            target: request.target.label(),
        });
    }
    let config = request.config_location()?;
    let files = vec![install_config(request, &config)?];
    Ok(report(request, files))
}

pub(crate) fn uninstall(request: &InstallRequest) -> Result<InstallReport, InstallError> {
    if !request.target.supports(request.location) {
        return Ok(report(request, Vec::new()));
    }
    let config = request.config_location()?;
    let mut files = vec![uninstall_config(request, &config)?];
    files.extend(uninstall_artifacts(request)?);
    if request.target == InstallTarget::Antigravity {
        let preferred = config.path;
        for alternate in [
            request.home.join(".gemini/config/mcp_config.json"),
            request.home.join(".gemini/antigravity/mcp_config.json"),
        ] {
            if alternate != preferred {
                let alternate = ConfigLocation {
                    path: alternate,
                    allowed_root: request.home.clone(),
                };
                let result = uninstall_json(&alternate, "mcpServers", None)?;
                if result.action == InstallAction::Removed {
                    files.push(result);
                }
            }
        }
    }
    Ok(report(request, files))
}

pub(crate) fn print_config(request: &InstallRequest) -> Result<String, InstallError> {
    if !request.target.supports(request.location) {
        return Ok(format!(
            "# {} has no project-local MCP configuration; use --location=global.\n",
            request.target.label()
        ));
    }
    let location = request.config_location()?;
    let body = match request.target {
        InstallTarget::Codex => {
            let mut document = DocumentMut::new();
            write_codex_table(&mut document, request)?;
            document.to_string()
        }
        InstallTarget::Claude if request.local() => {
            let project = path_text(&request.project_root)?;
            pretty_json(&json!({
                "projects": { (project): { "mcpServers": { "cartograph": entry(request)? } } }
            }))?
        }
        InstallTarget::Hermes => render_hermes_block(request),
        target => {
            let wrapper = wrapper_key(target);
            let mut root = Map::new();
            if target == InstallTarget::Opencode {
                root.insert(
                    "$schema".to_owned(),
                    Value::String(OPENCODE_SCHEMA.to_owned()),
                );
            }
            root.insert(
                wrapper.to_owned(),
                Value::Object(Map::from_iter([("cartograph".to_owned(), entry(request)?)])),
            );
            pretty_json(&Value::Object(root))?
        }
    };
    Ok(format!("# Add to {}\n\n{body}", location.path.display()))
}

fn report(request: &InstallRequest, files: Vec<InstallFileReport>) -> InstallReport {
    InstallReport {
        target: request.target,
        location: request.location,
        executable: request.command().to_owned(),
        project_root: request.project_root.clone(),
        files,
    }
}

fn install_config(
    request: &InstallRequest,
    config: &ConfigLocation,
) -> Result<InstallFileReport, InstallError> {
    match request.target {
        InstallTarget::Codex => install_codex(request, config),
        InstallTarget::Claude => install_claude(request, config),
        InstallTarget::Codebuddy | InstallTarget::Pi => {
            install_jsonc(request, config, wrapper_key(request.target))
        }
        InstallTarget::Hermes => install_hermes(request, config),
        target => install_json(request, config, wrapper_key(target)),
    }
}

fn uninstall_config(
    request: &InstallRequest,
    config: &ConfigLocation,
) -> Result<InstallFileReport, InstallError> {
    match request.target {
        InstallTarget::Codex => uninstall_codex(config),
        InstallTarget::Claude => uninstall_claude(request, config),
        InstallTarget::Codebuddy | InstallTarget::Pi => {
            uninstall_jsonc(config, wrapper_key(request.target))
        }
        InstallTarget::Hermes => uninstall_hermes(config),
        target => uninstall_json(config, wrapper_key(target), None),
    }
}

fn wrapper_key(target: InstallTarget) -> &'static str {
    match target {
        InstallTarget::Zed => "context_servers",
        InstallTarget::Opencode => "mcp",
        _ => "mcpServers",
    }
}

fn entry(request: &InstallRequest) -> Result<Value, InstallError> {
    let command = request.command();
    let args = request.server_args()?;
    let value = match request.target {
        InstallTarget::Codewhale | InstallTarget::Kimi | InstallTarget::Zed => {
            json!({"command": command, "args": args})
        }
        InstallTarget::Qoder => json!({"command": command, "args": args}),
        InstallTarget::Bob | InstallTarget::Reasonix => {
            json!({"command": command, "args": args, "disabled": false})
        }
        InstallTarget::Pi | InstallTarget::Rovo => {
            json!({"command": command, "args": args, "transport": "stdio"})
        }
        InstallTarget::Factory => {
            json!({"type": "stdio", "command": command, "args": args, "disabled": false})
        }
        InstallTarget::Copilot => {
            json!({"type": "stdio", "command": command, "args": args, "tools": ["*"]})
        }
        InstallTarget::Antigravity => {
            json!({"command": command, "args": ["serve", "--mcp"]})
        }
        InstallTarget::Opencode => {
            let mut command = vec![command.to_owned()];
            command.extend(args);
            json!({"type": "local", "command": command, "enabled": true})
        }
        _ => json!({"type": "stdio", "command": command, "args": args}),
    };
    Ok(value)
}

fn install_json(
    request: &InstallRequest,
    config: &ConfigLocation,
    wrapper: &str,
) -> Result<InstallFileReport, InstallError> {
    let prior = read_optional_config(&config.path)?;
    let mut root = match parse_json_object(prior.as_deref()) {
        Ok(root) => root,
        Err(InstallError::InvalidConfig)
            if prior
                .as_deref()
                .and_then(jsonc::parse)
                .and_then(|value| value.as_object().cloned())
                .is_some() =>
        {
            return install_jsonc(request, config, wrapper);
        }
        Err(error) => return Err(error),
    };
    if request.target == InstallTarget::Opencode && !root.contains_key("$schema") {
        root.insert(
            "$schema".to_owned(),
            Value::String(OPENCODE_SCHEMA.to_owned()),
        );
    }
    let servers = object_field(&mut root, wrapper)?;
    let desired = registration::merge_json_entry(
        servers.get("cartograph"),
        entry(request)?,
        request.command(),
    );
    if servers.get("cartograph") == Some(&desired) {
        return Ok(file_report(&config.path, InstallAction::Unchanged));
    }
    servers.insert("cartograph".to_owned(), desired);
    let rendered = pretty_json(&Value::Object(root))?;
    let action = if prior.is_some() {
        InstallAction::Updated
    } else {
        InstallAction::Created
    };
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, action))
}

fn uninstall_json(
    config: &ConfigLocation,
    wrapper: &str,
    nested_project: Option<&str>,
) -> Result<InstallFileReport, InstallError> {
    let Some(prior) = read_optional_config(&config.path)? else {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    };
    let mut root = match parse_json_object(Some(&prior)) {
        Ok(root) => root,
        Err(InstallError::InvalidConfig) if jsonc::parse(&prior).is_some() => {
            return uninstall_jsonc(config, wrapper);
        }
        Err(error) => return Err(error),
    };
    let removed = if let Some(project) = nested_project {
        root.get_mut("projects")
            .and_then(Value::as_object_mut)
            .and_then(|projects| projects.get_mut(project))
            .and_then(Value::as_object_mut)
            .and_then(|project| project.get_mut(wrapper))
            .and_then(Value::as_object_mut)
            .is_some_and(|servers| servers.remove("cartograph").is_some())
    } else {
        root.get_mut(wrapper)
            .and_then(Value::as_object_mut)
            .is_some_and(|servers| servers.remove("cartograph").is_some())
    };
    if !removed {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    }
    let rendered = pretty_json(&Value::Object(root))?;
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, InstallAction::Removed))
}

fn install_jsonc(
    request: &InstallRequest,
    config: &ConfigLocation,
    wrapper: &str,
) -> Result<InstallFileReport, InstallError> {
    let prior = read_optional_config(&config.path)?;
    let existing = prior
        .as_deref()
        .and_then(jsonc::parse)
        .and_then(|value| value.pointer(&format!("/{wrapper}/cartograph")).cloned());
    let desired =
        registration::merge_json_entry(existing.as_ref(), entry(request)?, request.command());
    if existing.as_ref() == Some(&desired) {
        return Ok(file_report(&config.path, InstallAction::Unchanged));
    }
    let rendered = match prior.as_deref() {
        Some(contents) if !contents.trim().is_empty() => {
            jsonc::upsert(contents, wrapper, &desired).ok_or(InstallError::InvalidConfig)?
        }
        _ => jsonc::fresh(wrapper, &desired).ok_or(InstallError::InvalidConfig)?,
    };
    let action = if prior.is_some() {
        InstallAction::Updated
    } else {
        InstallAction::Created
    };
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, action))
}

fn uninstall_jsonc(
    config: &ConfigLocation,
    wrapper: &str,
) -> Result<InstallFileReport, InstallError> {
    let Some(prior) = read_optional_config(&config.path)? else {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    };
    let exists = jsonc::parse(&prior)
        .and_then(|value| value.pointer(&format!("/{wrapper}/cartograph")).cloned())
        .is_some();
    if !exists {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    }
    let rendered = jsonc::remove(&prior, wrapper).ok_or(InstallError::InvalidConfig)?;
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, InstallAction::Removed))
}

fn install_claude(
    request: &InstallRequest,
    config: &ConfigLocation,
) -> Result<InstallFileReport, InstallError> {
    if !request.local() {
        return install_json(request, config, "mcpServers");
    }
    let prior = read_optional_config(&config.path)?;
    let mut root = parse_json_object(prior.as_deref())?;
    let projects = object_field(&mut root, "projects")?;
    let project_key = path_text(&request.project_root)?.to_owned();
    let project = projects
        .entry(project_key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(InstallError::InvalidConfig)?;
    let servers = object_field(project, "mcpServers")?;
    let desired = registration::merge_json_entry(
        servers.get("cartograph"),
        entry(request)?,
        request.command(),
    );
    if servers.get("cartograph") == Some(&desired) {
        return Ok(file_report(&config.path, InstallAction::Unchanged));
    }
    servers.insert("cartograph".to_owned(), desired);
    let rendered = pretty_json(&Value::Object(root))?;
    let action = if prior.is_some() {
        InstallAction::Updated
    } else {
        InstallAction::Created
    };
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, action))
}

fn uninstall_claude(
    request: &InstallRequest,
    config: &ConfigLocation,
) -> Result<InstallFileReport, InstallError> {
    let project = request
        .local()
        .then(|| path_text(&request.project_root))
        .transpose()?;
    uninstall_json(config, "mcpServers", project)
}

fn install_codex(
    request: &InstallRequest,
    config: &ConfigLocation,
) -> Result<InstallFileReport, InstallError> {
    let prior = read_optional_config(&config.path)?;
    let mut document = match prior.as_deref() {
        Some(contents) => contents
            .parse::<DocumentMut>()
            .map_err(|_| InstallError::InvalidConfig)?,
        None => DocumentMut::new(),
    };
    let old = document.to_string();
    write_codex_table(&mut document, request)?;
    let rendered = document.to_string();
    if rendered == old && prior.is_some() {
        return Ok(file_report(&config.path, InstallAction::Unchanged));
    }
    let action = if prior.is_some() {
        InstallAction::Updated
    } else {
        InstallAction::Created
    };
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, action))
}

fn write_codex_table(
    document: &mut DocumentMut,
    request: &InstallRequest,
) -> Result<(), InstallError> {
    ensure_table(document, "mcp_servers")?;
    let servers = document["mcp_servers"]
        .as_table_mut()
        .ok_or(InstallError::InvalidConfig)?;
    let server_args = request.server_args()?;
    if let Some(existing) = servers
        .get_mut("cartograph")
        .and_then(Item::as_table_like_mut)
    {
        registration::merge_codex_server(
            existing,
            &registration::DesiredServer {
                command: request.command(),
                args: &server_args,
            },
        );
        return Ok(());
    }
    let mut server = Table::new();
    server["command"] = value(request.command());
    let mut args = Array::new();
    for argument in server_args {
        args.push(argument);
    }
    server["args"] = Item::Value(toml_edit::Value::Array(args));
    servers["cartograph"] = Item::Table(server);
    Ok(())
}

fn uninstall_codex(config: &ConfigLocation) -> Result<InstallFileReport, InstallError> {
    let Some(prior) = read_optional_config(&config.path)? else {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    };
    let mut document = prior
        .parse::<DocumentMut>()
        .map_err(|_| InstallError::InvalidConfig)?;
    let removed = document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_mut)
        .is_some_and(|servers| servers.remove("cartograph").is_some());
    if !removed {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    }
    write_private_config(config, document.to_string().as_bytes())?;
    Ok(file_report(&config.path, InstallAction::Removed))
}

fn install_hermes(
    request: &InstallRequest,
    config: &ConfigLocation,
) -> Result<InstallFileReport, InstallError> {
    let prior = read_optional_config(&config.path)?.unwrap_or_default();
    let desired_server = render_hermes_server(request);
    if prior.contains(desired_server.trim_end())
        && prior.lines().any(|line| line.trim() == "- mcp-cartograph")
    {
        return Ok(file_report(&config.path, InstallAction::Unchanged));
    }
    let without_server = remove_yaml_top_child(&prior, "mcp_servers", "cartograph");
    let with_server = append_yaml_child(&without_server, "mcp_servers", &desired_server);
    let without_tool = remove_yaml_list_item(&with_server, HERMES_TOOLSET);
    let rendered = append_yaml_list_item(&without_tool, HERMES_TOOLSET);
    if rendered == prior {
        return Ok(file_report(&config.path, InstallAction::Unchanged));
    }
    let action = if prior.is_empty() {
        InstallAction::Created
    } else {
        InstallAction::Updated
    };
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, action))
}

fn uninstall_hermes(config: &ConfigLocation) -> Result<InstallFileReport, InstallError> {
    let Some(prior) = read_optional_config(&config.path)? else {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    };
    let without_server = remove_yaml_top_child(&prior, "mcp_servers", "cartograph");
    let rendered = remove_yaml_list_item(&without_server, HERMES_TOOLSET);
    if rendered == prior {
        return Ok(file_report(&config.path, InstallAction::NotFound));
    }
    write_private_config(config, rendered.as_bytes())?;
    Ok(file_report(&config.path, InstallAction::Removed))
}

fn render_hermes_block(request: &InstallRequest) -> String {
    format!(
        "mcp_servers:\n{}platform_toolsets:\n  cli:\n    - mcp-cartograph\n",
        render_hermes_server(request)
    )
}

fn render_hermes_server(request: &InstallRequest) -> String {
    format!(
        "  cartograph:\n    command: {}\n    args:\n      - serve\n      - --mcp\n",
        yaml_quote(request.command())
    )
}

fn remove_yaml_top_child(text: &str, parent: &str, child: &str) -> String {
    let lines = normalized_lines(text);
    let Some((parent_start, parent_end)) = yaml_top_range(&lines, parent) else {
        return ensure_trailing_newline(text);
    };
    let prefix = format!("  {child}:");
    let Some(child_start) =
        (parent_start + 1..parent_end).find(|index| lines[*index].trim_end() == prefix)
    else {
        return ensure_trailing_newline(text);
    };
    let child_end = (child_start + 1..parent_end)
        .find(|index| {
            let line = &lines[*index];
            !line.trim().is_empty() && line.starts_with("  ") && !line.starts_with("    ")
        })
        .unwrap_or(parent_end);
    let mut retained = lines;
    retained.drain(child_start..child_end);
    join_lines(retained)
}

fn append_yaml_child(text: &str, parent: &str, child_block: &str) -> String {
    let mut lines = normalized_lines(text);
    let block = normalized_lines(child_block);
    if let Some((_, parent_end)) = yaml_top_range(&lines, parent) {
        lines.splice(parent_end..parent_end, block);
    } else {
        if lines.iter().any(|line| !line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("{parent}:"));
        lines.extend(block);
    }
    join_lines(lines)
}

#[derive(Clone, Copy)]
struct YamlListItem<'value> {
    parent: &'value str,
    child: &'value str,
    item: &'value str,
}

const HERMES_TOOLSET: YamlListItem<'static> = YamlListItem {
    parent: "platform_toolsets",
    child: "cli",
    item: "mcp-cartograph",
};

fn remove_yaml_list_item(text: &str, input: YamlListItem<'_>) -> String {
    let YamlListItem {
        parent,
        child,
        item,
    } = input;
    let mut lines = normalized_lines(text);
    let Some((parent_start, parent_end)) = yaml_top_range(&lines, parent) else {
        return ensure_trailing_newline(text);
    };
    let child_line = format!("  {child}:");
    let Some(child_start) =
        (parent_start + 1..parent_end).find(|index| lines[*index].trim_end() == child_line)
    else {
        return ensure_trailing_newline(text);
    };
    let child_end = (child_start + 1..parent_end)
        .find(|index| {
            let line = &lines[*index];
            !line.trim().is_empty()
                && line.starts_with("  ")
                && !line.trim_start().starts_with("- ")
                && !line.starts_with("    ")
        })
        .unwrap_or(parent_end);
    let expected = format!("- {item}");
    lines.retain_with_index(|index, line| {
        !(index > child_start && index < child_end && line.trim() == expected)
    });
    join_lines(lines)
}

fn append_yaml_list_item(text: &str, input: YamlListItem<'_>) -> String {
    let YamlListItem {
        parent,
        child,
        item,
    } = input;
    let mut lines = normalized_lines(text);
    if let Some((parent_start, parent_end)) = yaml_top_range(&lines, parent) {
        let child_line = format!("  {child}:");
        if let Some(child_start) =
            (parent_start + 1..parent_end).find(|index| lines[*index].trim_end() == child_line)
        {
            lines.insert(child_start + 1, format!("    - {item}"));
        } else {
            lines.splice(
                parent_end..parent_end,
                [child_line, format!("    - {item}")],
            );
        }
    } else {
        if lines.iter().any(|line| !line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.extend([
            format!("{parent}:"),
            format!("  {child}:"),
            format!("    - {item}"),
        ]);
    }
    join_lines(lines)
}

trait RetainWithIndex<T> {
    fn retain_with_index(&mut self, keep: impl FnMut(usize, &T) -> bool);
}

impl<T> RetainWithIndex<T> for Vec<T> {
    fn retain_with_index(&mut self, mut keep: impl FnMut(usize, &T) -> bool) {
        let mut index = 0_usize;
        self.retain(|value| {
            let retain = keep(index, value);
            index = index.saturating_add(1);
            retain
        });
    }
}

fn yaml_top_range(lines: &[String], key: &str) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|line| line.trim_end() == format!("{key}:"))?;
    let end = (start + 1..lines.len())
        .find(|index| {
            let line = &lines[*index];
            !line.trim().is_empty()
                && !line.starts_with(char::is_whitespace)
                && line.trim_end().ends_with(':')
        })
        .unwrap_or(lines.len());
    Some((start, end))
}

fn normalized_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn join_lines(mut lines: Vec<String>) -> String {
    while lines.last().is_some_and(std::string::String::is_empty) {
        lines.pop();
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn yaml_quote(value: &str) -> String {
    format!("{value:?}")
}

fn install_claude_artifacts(
    request: &InstallRequest,
) -> Result<Vec<InstallFileReport>, InstallError> {
    let mut files = Vec::new();
    if request.permissions {
        files.push(upsert_permissions(request, claude_settings_path(request))?);
    }
    files.push(write_marked(
        request,
        claude_instructions_path(request),
        None,
    )?);
    files.push(write_owned(
        request,
        claude_skill_path(request),
        SKILL_TEMPLATE,
    )?);
    Ok(files)
}

fn install_cursor_artifacts(
    request: &InstallRequest,
) -> Result<Vec<InstallFileReport>, InstallError> {
    if !request.local() {
        return Ok(Vec::new());
    }
    let prefix = "---\ndescription: Cartograph MCP usage guide\nalwaysApply: true\n---\n\n";
    Ok(vec![write_marked(
        request,
        request.project_root.join(".cursor/rules/cartograph.mdc"),
        Some(prefix),
    )?])
}

fn install_codex_artifacts(
    request: &InstallRequest,
) -> Result<Vec<InstallFileReport>, InstallError> {
    let mut files = Vec::new();
    if !request.local() {
        files.push(write_marked(
            request,
            request.home.join(".codex/AGENTS.md"),
            None,
        )?);
    }
    let root = if request.local() {
        &request.project_root
    } else {
        &request.home
    };
    files.push(write_owned(
        request,
        root.join(".codex/skills/cartograph/SKILL.md"),
        SKILL_TEMPLATE,
    )?);
    Ok(files)
}

fn install_opencode_artifacts(
    request: &InstallRequest,
) -> Result<Vec<InstallFileReport>, InstallError> {
    let path = if request.local() {
        request
            .project_root
            .join(".opencode/commands/cartograph.md")
    } else {
        request.xdg_root()?.join("opencode/commands/cartograph.md")
    };
    let body = format!(
        "---\ndescription: Load Cartograph v2 code-graph guidance\n---\n\nApply this guidance for the current session.\n\n{INSTRUCTIONS}\n"
    );
    Ok(vec![write_owned(request, path, &body)?])
}

fn install_gemini_artifacts(
    request: &InstallRequest,
) -> Result<Vec<InstallFileReport>, InstallError> {
    let path = if request.local() {
        request.project_root.join("GEMINI.md")
    } else {
        request.home.join(".gemini/GEMINI.md")
    };
    Ok(vec![write_marked(request, path, None)?])
}

fn install_kiro_artifacts(
    request: &InstallRequest,
) -> Result<Vec<InstallFileReport>, InstallError> {
    let root = if request.local() {
        &request.project_root
    } else {
        &request.home
    };
    Ok(vec![write_owned(
        request,
        root.join(".kiro/steering/cartograph.md"),
        &format!("{INSTRUCTIONS}\n"),
    )?])
}

fn install_qoder_artifacts(
    request: &InstallRequest,
) -> Result<Vec<InstallFileReport>, InstallError> {
    if request.permissions {
        Ok(vec![upsert_permissions(
            request,
            request.config_location()?.path,
        )?])
    } else {
        Ok(Vec::new())
    }
}

fn install_artifacts(request: &InstallRequest) -> Result<Vec<InstallFileReport>, InstallError> {
    match request.target {
        InstallTarget::Claude => install_claude_artifacts(request),
        InstallTarget::Cursor => install_cursor_artifacts(request),
        InstallTarget::Codex => install_codex_artifacts(request),
        InstallTarget::Opencode => install_opencode_artifacts(request),
        InstallTarget::Gemini => install_gemini_artifacts(request),
        InstallTarget::Kiro => install_kiro_artifacts(request),
        InstallTarget::Qoder => install_qoder_artifacts(request),
        _ => Ok(Vec::new()),
    }
}

fn uninstall_artifacts(request: &InstallRequest) -> Result<Vec<InstallFileReport>, InstallError> {
    let mut files = Vec::new();
    match request.target {
        InstallTarget::Claude => {
            files.push(remove_permissions(request, claude_settings_path(request))?);
            files.push(remove_marked(request, claude_instructions_path(request))?);
            files.push(remove_owned(request, claude_skill_path(request))?);
        }
        InstallTarget::Cursor if request.local() => files.push(remove_marked(
            request,
            request.project_root.join(".cursor/rules/cartograph.mdc"),
        )?),
        InstallTarget::Codex => {
            if !request.local() {
                files.push(remove_marked(
                    request,
                    request.home.join(".codex/AGENTS.md"),
                )?);
            }
            let root = if request.local() {
                &request.project_root
            } else {
                &request.home
            };
            files.push(remove_owned(
                request,
                root.join(".codex/skills/cartograph/SKILL.md"),
            )?);
        }
        InstallTarget::Opencode => {
            let path = if request.local() {
                request
                    .project_root
                    .join(".opencode/commands/cartograph.md")
            } else {
                request.xdg_root()?.join("opencode/commands/cartograph.md")
            };
            files.push(remove_owned(request, path)?);
        }
        InstallTarget::Gemini => {
            let path = if request.local() {
                request.project_root.join("GEMINI.md")
            } else {
                request.home.join(".gemini/GEMINI.md")
            };
            files.push(remove_marked(request, path)?);
        }
        InstallTarget::Kiro => {
            let root = if request.local() {
                &request.project_root
            } else {
                &request.home
            };
            files.push(remove_owned(
                request,
                root.join(".kiro/steering/cartograph.md"),
            )?);
        }
        InstallTarget::Qoder => files.push(remove_permissions(
            request,
            request.config_location()?.path,
        )?),
        _ => {}
    }
    Ok(files)
}

fn claude_settings_path(request: &InstallRequest) -> PathBuf {
    if request.local() {
        request.project_root.join(".claude/settings.local.json")
    } else {
        request.home.join(".claude/settings.json")
    }
}

fn claude_instructions_path(request: &InstallRequest) -> PathBuf {
    if request.local() {
        request.project_root.join("CLAUDE.local.md")
    } else {
        request.home.join(".claude/CLAUDE.md")
    }
}

fn claude_skill_path(request: &InstallRequest) -> PathBuf {
    if request.local() {
        request
            .project_root
            .join(".claude/skills/cartograph/SKILL.md")
    } else {
        request.home.join(".claude/skills/cartograph/SKILL.md")
    }
}

fn upsert_permissions(
    request: &InstallRequest,
    path: PathBuf,
) -> Result<InstallFileReport, InstallError> {
    let config = artifact_location(request, path)?;
    let path = &config.path;
    let prior = read_optional_config(path)?;
    let mut root = parse_json_object(prior.as_deref())?;
    let permissions = object_field(&mut root, "permissions")?;
    let allow = permissions
        .entry("allow".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or(InstallError::InvalidConfig)?;
    let before = allow.clone();
    allow.retain(Value::is_string);
    for permission in PERMISSIONS {
        let value = Value::String(permission.to_owned());
        if !allow.contains(&value) {
            allow.push(value);
        }
    }
    if prior.is_some() && before == *allow {
        return Ok(file_report(path, InstallAction::Unchanged));
    }
    let rendered = pretty_json(&Value::Object(root))?;
    let action = if prior.is_some() {
        InstallAction::Updated
    } else {
        InstallAction::Created
    };
    write_private_config(&config, rendered.as_bytes())?;
    Ok(file_report(path, action))
}

fn remove_permissions(
    request: &InstallRequest,
    path: PathBuf,
) -> Result<InstallFileReport, InstallError> {
    let config = artifact_location(request, path)?;
    let path = &config.path;
    let Some(prior) = read_optional_config(path)? else {
        return Ok(file_report(path, InstallAction::NotFound));
    };
    let mut root = parse_json_object(Some(&prior))?;
    let Some(allow) = root
        .get_mut("permissions")
        .and_then(Value::as_object_mut)
        .and_then(|permissions| permissions.get_mut("allow"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(file_report(path, InstallAction::NotFound));
    };
    let before = allow.len();
    allow.retain(|value| {
        value
            .as_str()
            .is_none_or(|permission| !permission.starts_with("mcp__cartograph__"))
    });
    if before == allow.len() {
        return Ok(file_report(path, InstallAction::NotFound));
    }
    let rendered = pretty_json(&Value::Object(root))?;
    write_private_config(&config, rendered.as_bytes())?;
    Ok(file_report(path, InstallAction::Removed))
}

fn write_marked(
    request: &InstallRequest,
    path: PathBuf,
    prefix: Option<&str>,
) -> Result<InstallFileReport, InstallError> {
    let config = artifact_location(request, path)?;
    let path = &config.path;
    let prior = read_optional_config(path)?;
    let rendered = replace_marked(prior.as_deref().unwrap_or_default(), INSTRUCTIONS, prefix)?;
    if prior.as_deref() == Some(rendered.as_str()) {
        return Ok(file_report(path, InstallAction::Unchanged));
    }
    write_guidance(&config, rendered.as_bytes())?;
    Ok(file_report(
        path,
        if prior.is_some() {
            InstallAction::Updated
        } else {
            InstallAction::Created
        },
    ))
}

fn remove_marked(
    request: &InstallRequest,
    path: PathBuf,
) -> Result<InstallFileReport, InstallError> {
    let config = artifact_location(request, path)?;
    let path = &config.path;
    let Some(prior) = read_optional_config(path)? else {
        return Ok(file_report(path, InstallAction::NotFound));
    };
    let Some(start) = prior.find(SECTION_START) else {
        return Ok(file_report(path, InstallAction::NotFound));
    };
    let Some(relative_end) = prior[start..].find(SECTION_END) else {
        return Err(InstallError::InvalidConfig);
    };
    let end = start
        .saturating_add(relative_end)
        .saturating_add(SECTION_END.len());
    let mut rendered = format!("{}{}", &prior[..start], &prior[end..]);
    rendered = rendered.trim_matches('\n').to_owned();
    if !rendered.is_empty() {
        rendered.push('\n');
    }
    write_guidance(&config, rendered.as_bytes())?;
    Ok(file_report(path, InstallAction::Removed))
}

fn replace_marked(prior: &str, body: &str, prefix: Option<&str>) -> Result<String, InstallError> {
    if let Some(start) = prior.find(SECTION_START) {
        let relative_end = prior[start..]
            .find(SECTION_END)
            .ok_or(InstallError::InvalidConfig)?;
        let end = start
            .saturating_add(relative_end)
            .saturating_add(SECTION_END.len());
        return Ok(format!("{}{}{}", &prior[..start], body, &prior[end..]));
    }
    let mut rendered = prior.to_owned();
    if rendered.is_empty()
        && let Some(prefix) = prefix
    {
        rendered.push_str(prefix);
    }
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    if !rendered.is_empty() && !rendered.ends_with("\n\n") {
        rendered.push('\n');
    }
    rendered.push_str(body);
    rendered.push('\n');
    Ok(rendered)
}

fn write_owned(
    request: &InstallRequest,
    path: PathBuf,
    body: &str,
) -> Result<InstallFileReport, InstallError> {
    let config = artifact_location(request, path)?;
    let path = &config.path;
    let prior = read_optional_config(path)?;
    let mut rendered = body.to_owned();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    if prior.as_deref() == Some(rendered.as_str()) {
        return Ok(file_report(path, InstallAction::Unchanged));
    }
    write_guidance(&config, rendered.as_bytes())?;
    Ok(file_report(
        path,
        if prior.is_some() {
            InstallAction::Updated
        } else {
            InstallAction::Created
        },
    ))
}

fn remove_owned(
    request: &InstallRequest,
    path: PathBuf,
) -> Result<InstallFileReport, InstallError> {
    let config = artifact_location(request, path)?;
    let path = &config.path;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(file_report(path, InstallAction::NotFound));
        }
        Err(_) => return Err(InstallError::ReadConfig),
    };
    if !metadata.file_type().is_file() {
        return Err(InstallError::UnsafeConfig);
    }
    let parent = safe_config_parent(path, &config.allowed_root)?;
    fs::remove_file(path).map_err(|_| InstallError::WriteConfig)?;
    #[cfg(unix)]
    sync_parent(parent)?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(file_report(path, InstallAction::Removed))
}

fn artifact_location(
    request: &InstallRequest,
    path: PathBuf,
) -> Result<ConfigLocation, InstallError> {
    let allowed_root = if path.starts_with(&request.project_root) {
        request.project_root.clone()
    } else if path.starts_with(&request.home) {
        request.home.clone()
    } else if request.target == InstallTarget::Opencode
        && !request.local()
        && path.starts_with(request.xdg_root()?)
    {
        request.xdg_root()?
    } else {
        return Err(InstallError::UnsafeConfig);
    };
    Ok(ConfigLocation { path, allowed_root })
}

fn ensure_project_config_ignored(
    request: &InstallRequest,
    files: &[InstallFileReport],
) -> Result<(), InstallError> {
    let mut patterns = files
        .iter()
        .filter_map(|file| file.path.strip_prefix(&request.project_root).ok())
        .filter(|path| path != &Path::new(".gitignore"))
        .filter_map(|path| path.to_str())
        .map(|path| format!("/{path}"))
        .collect::<Vec<_>>();
    patterns.sort();
    patterns.dedup();
    if patterns.is_empty() {
        return Ok(());
    }
    let path = request.project_root.join(".gitignore");
    let prior = read_optional_config(&path)?.unwrap_or_default();
    if prior.contains('\0') {
        return Err(InstallError::UnsafeConfig);
    }
    let mut rendered = prior.clone();
    let missing = patterns
        .into_iter()
        .filter(|pattern| !prior.lines().any(|line| line.trim() == pattern))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    if !rendered.lines().any(|line| line.trim() == IGNORE_COMMENT) {
        rendered.push_str(IGNORE_COMMENT);
        rendered.push('\n');
    }
    for pattern in missing {
        rendered.push_str(&pattern);
        rendered.push('\n');
    }
    write_atomic(&AtomicWrite {
        path: &path,
        contents: rendered.as_bytes(),
        allowed_root: &request.project_root,
        unix_mode: IGNORE_FILE_MODE,
    })
}

fn file_report(path: &Path, action: InstallAction) -> InstallFileReport {
    InstallFileReport {
        path: path.to_owned(),
        action,
    }
}

fn canonical_directory(path: &Path, error: InstallError) -> Result<PathBuf, InstallError> {
    let canonical = path.canonicalize().map_err(|_| error)?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(error)
    }
}

fn host_home() -> Result<PathBuf, InstallError> {
    let raw = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or(InstallError::HostHome)?;
    canonical_directory(&PathBuf::from(raw), InstallError::HostHome)
}

fn env_root(name: &str, home: &Path, fallback: &str) -> Result<PathBuf, InstallError> {
    let Some(raw) = env::var_os(name) else {
        return Ok(home.join(fallback));
    };
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map_err(|_| InstallError::HostHome)?
            .join(path)
    };
    if absolute.exists() {
        canonical_directory(&absolute, InstallError::HostHome)
    } else if absolute.starts_with(home) {
        Ok(absolute)
    } else {
        Err(InstallError::UnsafeConfig)
    }
}

fn validate_command(command: &str) -> Result<&str, InstallError> {
    let command = command.trim();
    if command.is_empty()
        || command.len() > MAX_COMMAND_BYTES
        || command.contains(['\0', '\n', '\r'])
    {
        return Err(InstallError::InvalidCommand);
    }
    Ok(command)
}

fn strip_yaml_scalar(value: &str) -> &str {
    let without_comment = value
        .find(" #")
        .map_or(value, |comment| &value[..comment])
        .trim();
    if without_comment.len() >= 2 {
        let bytes = without_comment.as_bytes();
        if matches!(
            (bytes[0], bytes[bytes.len() - 1]),
            (b'"', b'"') | (b'\'', b'\'')
        ) {
            return &without_comment[1..without_comment.len() - 1];
        }
    }
    without_comment
}

fn ensure_table(document: &mut DocumentMut, key: &str) -> Result<(), InstallError> {
    if document.get(key).is_none() {
        document[key] = Item::Table(Table::new());
    }
    document[key]
        .as_table()
        .map(|_| ())
        .ok_or(InstallError::InvalidConfig)
}

fn parse_json_object(prior: Option<&str>) -> Result<Map<String, Value>, InstallError> {
    match prior {
        Some(contents) => serde_json::from_str::<Value>(contents)
            .map_err(|_| InstallError::InvalidConfig)?
            .as_object()
            .cloned()
            .ok_or(InstallError::InvalidConfig),
        None => Ok(Map::new()),
    }
}

fn object_field<'a>(
    root: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, InstallError> {
    if !root.contains_key(key) {
        root.insert(key.to_owned(), Value::Object(Map::new()));
    }
    root.get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or(InstallError::InvalidConfig)
}

fn pretty_json(value: &Value) -> Result<String, InstallError> {
    serde_json::to_string_pretty(value)
        .map(|rendered| format!("{rendered}\n"))
        .map_err(|_| InstallError::InvalidConfig)
}

fn read_optional_config(path: &Path) -> Result<Option<String>, InstallError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(InstallError::ReadConfig),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(InstallError::UnsafeConfig);
    }
    fs::read_to_string(path)
        .map(Some)
        .map_err(|_| InstallError::ReadConfig)
}

fn write_private_config(config: &ConfigLocation, contents: &[u8]) -> Result<(), InstallError> {
    write_atomic(&AtomicWrite {
        path: &config.path,
        contents,
        allowed_root: &config.allowed_root,
        unix_mode: PRIVATE_CONFIG_MODE,
    })
}

fn write_guidance(config: &ConfigLocation, contents: &[u8]) -> Result<(), InstallError> {
    write_atomic(&AtomicWrite {
        path: &config.path,
        contents,
        allowed_root: &config.allowed_root,
        unix_mode: GUIDANCE_MODE,
    })
}

struct AtomicWrite<'input> {
    path: &'input Path,
    contents: &'input [u8],
    allowed_root: &'input Path,
    unix_mode: u32,
}

fn write_atomic(input: &AtomicWrite<'_>) -> Result<(), InstallError> {
    let &AtomicWrite {
        path,
        contents,
        allowed_root,
        unix_mode,
    } = input;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
        return Err(InstallError::UnsafeConfig);
    }
    let parent = safe_config_parent(path, allowed_root)?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err(InstallError::UnsafeConfig);
    }
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| InstallError::WriteConfig)?;
    #[cfg(unix)]
    set_atomic_permissions(file.as_file(), unix_mode)?;
    #[cfg(not(unix))]
    let _ = unix_mode;
    file.write_all(contents)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|_| InstallError::WriteConfig)?;
    file.persist(path).map_err(|_| InstallError::WriteConfig)?;
    #[cfg(unix)]
    sync_parent(parent)?;
    Ok(())
}

#[cfg(unix)]
fn set_atomic_permissions(file: &fs::File, unix_mode: u32) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(unix_mode))
        .map_err(|_| InstallError::WriteConfig)
}

fn safe_config_parent<'a>(path: &'a Path, allowed_root: &Path) -> Result<&'a Path, InstallError> {
    let parent = path.parent().ok_or(InstallError::WriteConfig)?;
    create_safe_directories(parent, allowed_root)?;
    let metadata = fs::symlink_metadata(parent).map_err(|_| InstallError::WriteConfig)?;
    if !metadata.file_type().is_dir() {
        return Err(InstallError::UnsafeConfig);
    }
    let canonical = parent
        .canonicalize()
        .map_err(|_| InstallError::WriteConfig)?;
    let allowed = allowed_root
        .canonicalize()
        .map_err(|_| InstallError::WriteConfig)?;
    if !canonical.starts_with(allowed) {
        return Err(InstallError::UnsafeConfig);
    }
    Ok(parent)
}

fn create_safe_directories(path: &Path, allowed_root: &Path) -> Result<(), InstallError> {
    let allowed = allowed_root
        .canonicalize()
        .map_err(|_| InstallError::WriteConfig)?;
    let relative = path
        .strip_prefix(allowed_root)
        .map_err(|_| InstallError::UnsafeConfig)?;
    let mut current = allowed;
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(InstallError::UnsafeConfig),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|_| InstallError::WriteConfig)?;
            }
            Err(_) => return Err(InstallError::WriteConfig),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), InstallError> {
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| InstallError::WriteConfig)
}

fn path_text(path: &Path) -> Result<&str, InstallError> {
    path.to_str().ok_or(InstallError::NonUtf8Path)
}

fn ensure_trailing_newline(text: &str) -> String {
    if text.is_empty() || text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(crate) enum InstallError {
    #[error("project path must be an existing directory")]
    ProjectRoot,
    #[error("Cartograph executable path must be an existing regular file")]
    Executable,
    #[error("agent host home must be an existing directory")]
    HostHome,
    #[error("agent configuration path is a symlink, non-file, outside its scope, or exceeds 1 MiB")]
    UnsafeConfig,
    #[error("could not read the agent configuration")]
    ReadConfig,
    #[error("agent configuration is not valid for the selected target")]
    InvalidConfig,
    #[error("could not write the agent configuration")]
    WriteConfig,
    #[error("agent configuration requires UTF-8 project and executable paths")]
    NonUtf8Path,
    #[error("--command must be non-blank, single-line, and at most 4096 bytes")]
    InvalidCommand,
    #[error("{target} does not support project-local MCP configuration")]
    UnsupportedLocation { target: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn versioned_installations_register_the_stable_current_launcher() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("versioned launcher fixture failed: {error}"));
        let install_root = fixture.path().join(".cartograph-cli");
        let executable = install_root.join("versions/v2.1.4/bin/cartograph");
        fs::create_dir_all(
            executable
                .parent()
                .unwrap_or_else(|| panic!("versioned executable had no parent")),
        )
        .unwrap_or_else(|error| panic!("versioned executable parent failed: {error}"));
        fs::write(&executable, b"fixture")
            .unwrap_or_else(|error| panic!("versioned executable write failed: {error}"));
        symlink("versions/v2.1.4", install_root.join("current"))
            .unwrap_or_else(|error| panic!("current launcher link failed: {error}"));
        let canonical = executable.canonicalize().unwrap_or_else(|error| {
            panic!("versioned executable canonicalization failed: {error}")
        });

        assert_eq!(
            stable_registration_executable(&canonical),
            Some(
                install_root
                    .canonicalize()
                    .unwrap_or_else(|error| panic!("install root canonicalization failed: {error}"))
                    .join("current/bin/cartograph"),
            )
        );
    }

    fn fake_executable(root: &Path) -> PathBuf {
        let path = root.join("cartograph-test-bin");
        fs::write(&path, b"test executable")
            .unwrap_or_else(|error| panic!("fake executable write failed: {error}"));
        path
    }

    fn request<'a>(
        project: &'a Path,
        home: &'a Path,
        executable: &'a Path,
        target: InstallTarget,
        location: InstallLocation,
    ) -> InstallRequest {
        InstallRequest::new_for_test(&TestInstallRequest {
            project_root: project,
            executable,
            target,
            location,
            home,
            command_override: Some("/opt/cartograph/bin/cartograph"),
            permissions: true,
        })
        .unwrap_or_else(|error| panic!("request failed: {error}"))
    }

    #[test]
    fn every_target_has_a_safe_printable_config_in_each_supported_scope() {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let executable = fake_executable(project.path());
        for target in InstallTarget::ALL {
            for location in [InstallLocation::Global, InstallLocation::Local] {
                let request = request(project.path(), home.path(), &executable, target, location);
                let rendered = print_config(&request)
                    .unwrap_or_else(|error| panic!("{} print failed: {error}", target.label()));
                assert!(rendered.contains(target.label()) || rendered.contains("Add to"));
                assert!(!rendered.contains("postgresql://"));
                if target.supports(location) {
                    assert!(rendered.contains("cartograph"));
                    assert!(rendered.contains("serve"));
                    if location == InstallLocation::Local && target != InstallTarget::Antigravity {
                        assert!(
                            rendered.contains("--project-path") || target == InstallTarget::Hermes
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn local_managed_port_is_a_portable_server_argument() {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let executable = fake_executable(project.path());
        for target in [
            InstallTarget::Codex,
            InstallTarget::Claude,
            InstallTarget::Kimi,
        ] {
            let request = request(
                project.path(),
                home.path(),
                &executable,
                target,
                InstallLocation::Local,
            )
            .with_managed_database_port(55_435);
            let arguments = request
                .server_args()
                .unwrap_or_else(|error| panic!("{} arguments failed: {error}", target.label()));
            assert_eq!(
                arguments[arguments.len() - 2..],
                ["--managed-database-port", "55435"]
            );
            let rendered = print_config(&request)
                .unwrap_or_else(|error| panic!("{} print failed: {error}", target.label()));
            assert!(rendered.contains("--managed-database-port"));
            assert!(rendered.contains("55435"));
            assert!(!rendered.contains("CARTOGRAPH_DATABASE_URL"));
        }
    }

    #[test]
    fn all_supported_targets_install_idempotently_and_uninstall_only_owned_entries() {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let executable = fake_executable(project.path());
        for target in InstallTarget::ALL {
            for location in [InstallLocation::Global, InstallLocation::Local] {
                if !target.supports(location) {
                    continue;
                }
                let request = request(project.path(), home.path(), &executable, target, location);
                let first = install(&request)
                    .unwrap_or_else(|error| panic!("{} install failed: {error}", target.label()));
                let second = install(&request)
                    .unwrap_or_else(|error| panic!("{} repeat failed: {error}", target.label()));
                assert!(first.changed(), "{} did not install", target.label());
                assert!(!second.changed(), "{} was not idempotent", target.label());
                let removed = uninstall(&request)
                    .unwrap_or_else(|error| panic!("{} uninstall failed: {error}", target.label()));
                assert!(removed.changed(), "{} did not uninstall", target.label());
                let repeated = uninstall(&request).unwrap_or_else(|error| {
                    panic!("{} repeat uninstall failed: {error}", target.label())
                });
                assert!(
                    !repeated.changed(),
                    "{} uninstall was not idempotent",
                    target.label()
                );
            }
        }
    }

    #[test]
    fn codebuddy_preserves_jsonc_comments_and_unrelated_servers() {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let executable = fake_executable(project.path());
        let config = project.path().join(".mcp.json");
        fs::write(
            &config,
            "{\n  // retained\n  \"mcpServers\": { \"other\": { \"command\": \"x\" }, },\n}\n",
        )
        .unwrap_or_else(|error| panic!("seed failed: {error}"));
        let request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Codebuddy,
            InstallLocation::Local,
        );
        install(&request).unwrap_or_else(|error| panic!("install failed: {error}"));
        uninstall(&request).unwrap_or_else(|error| panic!("uninstall failed: {error}"));
        let after =
            fs::read_to_string(config).unwrap_or_else(|error| panic!("read failed: {error}"));
        assert!(after.contains("// retained"));
        assert!(after.contains("\"other\": { \"command\": \"x\" }"));
    }

    #[test]
    fn claude_local_scope_preserves_other_projects_and_permissions() {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let executable = fake_executable(project.path());
        fs::write(
            home.path().join(".claude.json"),
            "{\"projects\":{\"/retained\":{\"mcpServers\":{\"other\":{\"command\":\"x\"}}}}}",
        )
        .unwrap_or_else(|error| panic!("seed failed: {error}"));
        let request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Claude,
            InstallLocation::Local,
        );
        install(&request).unwrap_or_else(|error| panic!("install failed: {error}"));
        uninstall(&request).unwrap_or_else(|error| panic!("uninstall failed: {error}"));
        let root: Value = serde_json::from_str(
            &fs::read_to_string(home.path().join(".claude.json"))
                .unwrap_or_else(|error| panic!("read failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("parse failed: {error}"));
        assert_eq!(
            root.pointer("/projects/~1retained/mcpServers/other/command"),
            Some(&Value::String("x".to_owned()))
        );
    }

    const PINNED_COMMAND: &str = "/opt/cartograph/bin/cartograph";
    const STALE_EXECUTABLE: &str = "/old/release/bin/cartograph";

    fn seed(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|error| panic!("seed dir failed: {error}"));
        }
        fs::write(path, contents).unwrap_or_else(|error| panic!("seed failed: {error}"));
    }

    fn json_at(path: &Path, pointer: &str) -> Value {
        let root: Value = serde_json::from_str(
            &fs::read_to_string(path).unwrap_or_else(|error| panic!("read failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("parse failed: {error}"));
        root.pointer(pointer)
            .cloned()
            .unwrap_or_else(|| panic!("{pointer} was missing"))
    }

    fn registration_action(request: &InstallRequest) -> InstallAction {
        install(request)
            .unwrap_or_else(|error| panic!("install failed: {error}"))
            .files()[0]
            .action()
    }

    #[test]
    fn rewriting_a_wrapped_registration_repins_only_its_absolute_executable() {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let executable = fake_executable(project.path());

        let cursor = project.path().join(".cursor/mcp.json");
        let wrapped = json!({
            "command": "/usr/bin/env",
            "args": ["FIXTURE_MODE=opaque", STALE_EXECUTABLE, "serve", "--mcp", "--profile", "coding"],
            "env": {"EXAMPLE_FLAG": "1"},
            "cwd": "/srv/work",
            "hostSpecific": {"keep": true}
        });
        seed(
            &cursor,
            &json!({"mcpServers": {"cartograph": wrapped}}).to_string(),
        );
        let cursor_request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Cursor,
            InstallLocation::Local,
        );
        assert_eq!(registration_action(&cursor_request), InstallAction::Updated);
        let mut expected = wrapped;
        expected["args"][1] = json!(PINNED_COMMAND);
        assert_eq!(json_at(&cursor, "/mcpServers/cartograph"), expected);
        assert_eq!(
            registration_action(&cursor_request),
            InstallAction::Unchanged
        );

        let codex = project.path().join(".codex/config.toml");
        let codex_seed = format!(
            "[mcp_servers.cartograph]\ncommand = \"op\"\nargs = [\"run\", \"--\", \"{STALE_EXECUTABLE}\", \"serve\", \"--mcp\"] # wrapper\nstartup_timeout_sec = 30\n\n[mcp_servers.cartograph.env]\nEXAMPLE_FLAG = \"1\"\n"
        );
        seed(&codex, &codex_seed);
        let codex_request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Codex,
            InstallLocation::Local,
        );
        assert_eq!(registration_action(&codex_request), InstallAction::Updated);
        assert_eq!(
            fs::read_to_string(&codex).unwrap_or_else(|error| panic!("read failed: {error}")),
            codex_seed.replace(STALE_EXECUTABLE, PINNED_COMMAND)
        );

        let claude = home.path().join(".claude.json");
        let project_key = project
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("project canonicalization failed: {error}"))
            .to_string_lossy()
            .into_owned();
        let path_lookup = json!({"projects": {(project_key): {"mcpServers": {"cartograph": {
            "command": "direnv",
            "args": ["exec", ".", "cartograph", "serve", "--mcp"],
            "env": {"EXAMPLE_FLAG": "1"}
        }}}}})
        .to_string();
        seed(&claude, &path_lookup);
        let claude_request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Claude,
            InstallLocation::Local,
        );
        assert_eq!(
            registration_action(&claude_request),
            InstallAction::Unchanged
        );
        assert_eq!(
            fs::read_to_string(&claude).unwrap_or_else(|error| panic!("read failed: {error}")),
            path_lookup
        );
    }

    #[test]
    fn rewriting_a_direct_registration_keeps_unowned_keys_and_extra_arguments() {
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let executable = fake_executable(project.path());
        let project_root = project
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("project canonicalization failed: {error}"))
            .to_string_lossy()
            .into_owned();

        let cursor = project.path().join(".cursor/mcp.json");
        seed(
            &cursor,
            &json!({"mcpServers": {"cartograph": {
                "command": STALE_EXECUTABLE,
                "args": ["serve", "--mcp", "--project-path", "/old/project", "--profile", "coding"],
                "env": {"EXAMPLE_FLAG": "1"},
                "cwd": "/srv/work",
                "hostSpecific": true
            }}})
            .to_string(),
        );
        let cursor_request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Cursor,
            InstallLocation::Local,
        );
        assert_eq!(registration_action(&cursor_request), InstallAction::Updated);
        assert_eq!(
            json_at(&cursor, "/mcpServers/cartograph"),
            json!({
                "type": "stdio",
                "command": PINNED_COMMAND,
                "args": ["serve", "--mcp", "--project-path", project_root, "--profile", "coding"],
                "env": {"EXAMPLE_FLAG": "1"},
                "cwd": "/srv/work",
                "hostSpecific": true
            })
        );
        assert_eq!(
            registration_action(&cursor_request),
            InstallAction::Unchanged
        );

        let codex = project.path().join(".codex/config.toml");
        seed(
            &codex,
            &format!(
                "[mcp_servers.cartograph]\ncommand = \"{STALE_EXECUTABLE}\"\nargs = [\"serve\", \"--mcp\", \"--no-auto-sync\"]\nstartup_timeout_sec = 30\n\n[mcp_servers.cartograph.env]\nEXAMPLE_FLAG = \"1\"\n"
            ),
        );
        let codex_request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Codex,
            InstallLocation::Local,
        );
        assert_eq!(registration_action(&codex_request), InstallAction::Updated);
        let document = fs::read_to_string(&codex)
            .unwrap_or_else(|error| panic!("read failed: {error}"))
            .parse::<DocumentMut>()
            .unwrap_or_else(|error| panic!("parse failed: {error}"));
        let server = &document["mcp_servers"]["cartograph"];
        assert_eq!(server["command"].as_str(), Some(PINNED_COMMAND));
        let args = server["args"].as_array().map(|args| {
            args.iter()
                .filter_map(toml_edit::Value::as_str)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            args,
            Some(vec![
                "serve",
                "--mcp",
                "--project-path",
                project_root.as_str(),
                "--no-auto-sync"
            ])
        );
        assert_eq!(server["startup_timeout_sec"].as_integer(), Some(30));
        assert_eq!(server["env"]["EXAMPLE_FLAG"].as_str(), Some("1"));
        assert_eq!(
            registration_action(&codex_request),
            InstallAction::Unchanged
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_configuration_directory() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project failed: {error}"));
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home failed: {error}"));
        let external =
            tempfile::tempdir().unwrap_or_else(|error| panic!("external failed: {error}"));
        let executable = fake_executable(project.path());
        symlink(external.path(), project.path().join(".cursor"))
            .unwrap_or_else(|error| panic!("symlink failed: {error}"));
        let request = request(
            project.path(),
            home.path(),
            &executable,
            InstallTarget::Cursor,
            InstallLocation::Local,
        );
        assert_eq!(install(&request), Err(InstallError::UnsafeConfig));
        assert!(!external.path().join("mcp.json").exists());
    }
}
