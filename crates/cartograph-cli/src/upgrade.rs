use std::{
    cmp::Ordering,
    env,
    ffi::OsString,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::Duration,
};

use cartograph_config::DATABASE_URL_ENV;
use cartograph_domain::GenerationId;
use futures_util::StreamExt as _;
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tempfile::{NamedTempFile, TempPath};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::host::{
    DiagnosticLocation, HostScope, InstallTargetDetection, detect_install_targets,
    detect_install_targets_in, host_home,
};
use crate::install::{self, InstallLocation, InstallRequest, InstallRequestInput, InstallTarget};
use index_child::{
    ChildStop, DEFAULT_INDEX_CHILD_POLICY, IndexChildOutcome, IndexChildPolicy, IndexChildReport,
    IndexChildTimeout,
};

mod index_child;

const REMOTE: &str = "https://github.com/adder-factory/cartograph.git";
const RELEASE_BASE: &str = "https://github.com/adder-factory/cartograph/releases/download";
const RELEASES_URL: &str = "https://github.com/adder-factory/cartograph/releases";
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/adder-factory/cartograph/releases/latest";
const MAXIMUM_TAG_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAXIMUM_CHECKSUM_BYTES: usize = 1024 * 1024;
const MAXIMUM_BINARY_BYTES: usize = 200 * 1024 * 1024;
const MAXIMUM_STAGED_BINARY_LAUNCH_ATTEMPTS: usize = 3;
const STAGED_BINARY_LAUNCH_RETRY_DELAY: Duration = Duration::from_millis(25);
const MANAGED_START_TIMEOUT: Duration = Duration::from_mins(15);
const PROJECT_VERIFICATION_TIMEOUT: Duration = Duration::from_mins(2);
/// Doctor and the next-process status each rescan the whole checkout (status
/// twice when the generation changes underneath it), so on a large project
/// they get a source-scan budget rather than a database probe's.
const SOURCE_VERIFICATION_TIMEOUT: Duration = Duration::from_mins(10);
const MAXIMUM_STATUS_PROBE_BYTES: usize = 8 * 1024 * 1024;
const LOWER_HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const HIGH_NIBBLE_SHIFT: u8 = 4;
const LOW_NIBBLE_MASK: u8 = 0x0f;
const REGISTRATION_REPAIR_TIMEOUT: Duration = Duration::from_mins(1);
/// `registrationRepair.changes[].outcome` once the entry launches the selected
/// executable.
const REPAIR_REPINNED: &str = "repinned";
/// `registrationRepair.changes[].outcome` when the entry still needs the
/// reported `manualStep`.
const REPAIR_MANUAL: &str = "manual";
/// `registrationRepair.changes[].field` of a direct pin.
const COMMAND_FIELD: &str = "command";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpgradeReport {
    status: &'static str,
    current_version: String,
    latest_version: Option<String>,
    installed_version: Option<String>,
    #[serde(flatten)]
    operation: UpgradeOperationState,
    restart_required: bool,
    message: String,
    next_steps: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    registration_repair: Option<RegistrationRepair>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_reconciliation: Option<ProjectReconciliation>,
    registrations: Vec<InstallTargetDetection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpgradeOperationState {
    apply_requested: bool,
    applied: bool,
    completed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationRepair {
    attempted: u16,
    repaired: u16,
    failed: u16,
    /// One entry per attempted registration, so no rewrite is silent.
    changes: Vec<RegistrationChange>,
}

/// What `upgrade --apply` changed, or could not change, in one registration.
/// Only executable paths and the wrapper command are reported; wrapper
/// arguments and `env` values never are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationChange {
    target: &'static str,
    location: &'static str,
    config_path: &'static str,
    /// `stale_absolute` for a direct pin, `wrapped` for a wrapper registration.
    command_state: &'static str,
    /// `repinned` or `manual`.
    outcome: &'static str,
    /// The entry field holding the Cartograph executable: `command` or `args[N]`.
    field: String,
    /// The unchanged wrapper `command` of a wrapped registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    wrapper: Option<String>,
    /// The stale Cartograph executable that was configured.
    from: String,
    /// The Cartograph executable the entry now launches (or should launch).
    to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    manual_step: Option<String>,
}

struct RegistrationRepairInput<'input> {
    /// Installed executable: it reinstalls direct pins, and its stable
    /// launcher is the path every repaired entry is pinned to.
    executable: &'input Path,
    scope: HostScope<'input>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpgradeStep {
    state: &'static str,
    message: String,
    /// Stable machine-readable cause, when the step has one: the index
    /// child's failure code, or which bound stopped it.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectReconciliation {
    /// `ready`, `source_changed` (complete, but edits continued after
    /// publication), or `blocked`.
    state: &'static str,
    /// Rerunning the same upgrade command, without another action, is the
    /// next step: a bounded wait or timeout ended without a failure verdict.
    retryable: bool,
    database: UpgradeStep,
    index: UpgradeStep,
    doctor: UpgradeStep,
    verification: UpgradeStep,
    fresh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    managed_database_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_confirmation: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectDatabaseMode {
    External,
    Managed(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectCommand {
    ManagedStatus,
    ManagedStart,
    Index,
    Doctor,
    Status,
}

impl ProjectCommand {
    /// Wall-clock bound for a plain child; the index child is instead
    /// supervised by progress under [`index_child::DEFAULT_INDEX_CHILD_POLICY`].
    const fn timeout(self) -> Duration {
        match self {
            Self::ManagedStart => MANAGED_START_TIMEOUT,
            Self::Index => index_child::INDEX_ABSOLUTE_CEILING,
            Self::ManagedStatus => PROJECT_VERIFICATION_TIMEOUT,
            Self::Doctor | Self::Status => SOURCE_VERIFICATION_TIMEOUT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectProcessOutcome {
    Succeeded,
    Failed,
    TimedOut,
}

/// `projectReconciliation.state` once every step passed and status is fresh.
const RECONCILED_FRESH: &str = "ready";
/// `projectReconciliation.state` (and the index/verification step state) when
/// the installed binary published a complete current generation but the
/// checkout changed after publication.
const RECONCILED_SOURCE_CHANGED: &str = "source_changed";
/// Index step state when another Cartograph operation (a project writer, or
/// a schema maintenance step that refuses new leases) kept the project busy
/// throughout the child's bounded wait.
const ANOTHER_WRITER_ACTIVE: &str = "another_writer_active";
/// Index child failure code for a project that stayed busy.
const LEASE_BUSY_CODE: &str = "lease_busy";

struct ProjectProcessInput<'input> {
    executable: &'input Path,
    project_path: &'input Path,
    database_mode: ProjectDatabaseMode,
    command: ProjectCommand,
}

struct CompletionInput<'project> {
    running_version: String,
    latest_version: String,
    installed_version: String,
    binary_applied: bool,
    installed: InstalledBinary,
    project_path: &'project Path,
    registrations_before: Vec<InstallTargetDetection>,
}

#[derive(Clone, Copy)]
struct CompletionGuidance<'report> {
    launcher_warning: Option<&'report str>,
    project_reconciliation: Option<&'report ProjectReconciliation>,
    registration_repair: &'report RegistrationRepair,
    completed: bool,
    host_configured: bool,
    restart_required: bool,
}

struct ResolvedUpgradeInput<'project> {
    apply: bool,
    current: Version,
    current_text: String,
    current_executable: Option<PathBuf>,
    latest: Version,
    latest_text: String,
    project_path: &'project Path,
    registrations: Vec<InstallTargetDetection>,
}

#[derive(Debug, PartialEq, Eq)]
struct InstalledBinary {
    path: PathBuf,
    launcher_warning: Option<String>,
}

struct UnknownReportInput<'reason> {
    current_version: String,
    apply: bool,
    reason: &'reason str,
    registrations: Vec<InstallTargetDetection>,
}

pub(super) fn render(report: &UpgradeReport) -> String {
    let mut output = format!("{}\n", report.message);
    for step in &report.next_steps {
        output.push_str("- ");
        output.push_str(step);
        output.push('\n');
    }
    if let Some(repair) = report.registration_repair.as_ref() {
        render_registration_repair(&mut output, repair);
    }
    render_registration_audit(&mut output, &report.registrations);
    output
}

fn render_registration_repair(output: &mut String, repair: &RegistrationRepair) {
    if repair.changes.is_empty() {
        return;
    }
    output.push_str("MCP registration repairs:\n");
    for change in &repair.changes {
        for part in [
            "- ",
            change.target,
            " ",
            change.location,
            " (",
            change.config_path,
            "): ",
        ] {
            output.push_str(part);
        }
        output.push_str(&registration_change_summary(change));
        output.push('\n');
    }
}

fn registration_change_summary(change: &RegistrationChange) -> String {
    if let Some(step) = change.manual_step.as_deref() {
        return format!("manual: {step}");
    }
    let preserved = change.wrapper.as_deref().map_or_else(
        || "; keys Cartograph does not own were preserved".to_owned(),
        |wrapper| {
            format!("; wrapper {wrapper}, its other arguments, and every other key were preserved")
        },
    );
    format!(
        "repinned {} {} -> {}{preserved}",
        change.field, change.from, change.to
    )
}

fn render_registration_audit(output: &mut String, registrations: &[InstallTargetDetection]) {
    let configured = registrations
        .iter()
        .filter(|registration| registration.cartograph_configured)
        .collect::<Vec<_>>();
    if !configured.is_empty() {
        output.push_str("MCP registration audit:\n");
        for registration in configured {
            output.push_str("- ");
            output.push_str(registration.target);
            output.push(' ');
            output.push_str(registration.location);
            output.push_str(": ");
            output.push_str(registration.command_state);
            if let Some(state) = registration.wrapped_executable_state {
                output.push_str(" (wrapped executable: ");
                output.push_str(state);
                output.push(')');
            }
            output.push_str(" (");
            output.push_str(registration.config_path);
            output.push_str(")\n");
            if let Some(command) = registration.repin_command.as_deref() {
                output.push_str("  Repin: ");
                output.push_str(command);
                output.push('\n');
            }
        }
    }
}

pub(super) fn succeeded(report: &UpgradeReport) -> bool {
    report.status != "blocked" && report.status != "unknown"
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<PrereleasePart>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PrereleasePart {
    Numeric(u64),
    Text(String),
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then_with(|| compare_prerelease(&self.prerelease, &other.prerelease))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(super) async fn run_upgrade(apply: bool, project_path: &Path) -> UpgradeReport {
    let current_text = env!("CARGO_PKG_VERSION").to_owned();
    let current_executable = env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok());
    let registrations = registration_audit(project_path, current_executable.as_deref());
    let Some(current) = parse_version(&current_text) else {
        return report_unknown(UnknownReportInput {
            current_version: current_text,
            apply,
            reason: "running version is not valid semver",
            registrations,
        });
    };
    let latest_text = match latest_version().await {
        Ok(version) => version,
        Err(message) => {
            return report_unknown(UnknownReportInput {
                current_version: current_text,
                apply,
                reason: &message,
                registrations,
            });
        }
    };
    let Some(latest) = parse_version(&latest_text) else {
        return report_unknown(UnknownReportInput {
            current_version: current_text,
            apply,
            reason: "published version is not valid semver",
            registrations,
        });
    };
    run_resolved_upgrade(ResolvedUpgradeInput {
        apply,
        current,
        current_text,
        current_executable,
        latest,
        latest_text,
        project_path,
        registrations,
    })
    .await
}

async fn run_resolved_upgrade(input: ResolvedUpgradeInput<'_>) -> UpgradeReport {
    if !input.apply {
        return report_upgrade_audit(input);
    }
    if input.current >= input.latest {
        complete_current_upgrade(input).await
    } else {
        install_and_complete_upgrade(input).await
    }
}

fn report_upgrade_audit(input: ResolvedUpgradeInput<'_>) -> UpgradeReport {
    let ResolvedUpgradeInput {
        current,
        current_text,
        latest,
        latest_text,
        registrations,
        ..
    } = input;
    if current >= latest {
        return UpgradeReport {
            status: "current",
            current_version: current_text.clone(),
            latest_version: Some(latest_text),
            installed_version: Some(current_text.clone()),
            operation: UpgradeOperationState {
                apply_requested: false,
                applied: false,
                completed: true,
            },
            restart_required: false,
            message: format!("Cartograph {current_text} is current."),
            next_steps: registration_next_steps(&registrations),
            registration_repair: None,
            project_reconciliation: None,
            registrations,
        };
    }
    UpgradeReport {
        status: "update_available",
        current_version: current_text.clone(),
        latest_version: Some(latest_text.clone()),
        installed_version: Some(current_text.clone()),
        operation: UpgradeOperationState {
            apply_requested: false,
            applied: false,
            completed: false,
        },
        restart_required: false,
        message: format!("Cartograph {current_text} -> {latest_text} is available."),
        next_steps: vec![
            "Run `cartograph upgrade --apply --project-path <path>` once; it installs the verified release, repairs owned registrations, migrates safe schema changes, refreshes the index, and verifies a next-process status from the installed binary."
                .to_owned(),
        ],
        registration_repair: None,
        project_reconciliation: None,
        registrations,
    }
}

async fn complete_current_upgrade(input: ResolvedUpgradeInput<'_>) -> UpgradeReport {
    let ResolvedUpgradeInput {
        current_text,
        current_executable,
        latest_text,
        project_path,
        registrations,
        ..
    } = input;
    let Some(path) = current_executable else {
        return unresolved_current_executable_report(current_text, latest_text, registrations);
    };
    complete_upgrade(CompletionInput {
        running_version: current_text.clone(),
        latest_version: latest_text,
        installed_version: current_text,
        binary_applied: false,
        installed: InstalledBinary {
            path,
            launcher_warning: None,
        },
        project_path,
        registrations_before: registrations,
    })
    .await
}

fn unresolved_current_executable_report(
    current_text: String,
    latest_text: String,
    registrations: Vec<InstallTargetDetection>,
) -> UpgradeReport {
    UpgradeReport {
        status: "blocked",
        current_version: current_text.clone(),
        latest_version: Some(latest_text),
        installed_version: Some(current_text),
        operation: UpgradeOperationState {
            apply_requested: true,
            applied: false,
            completed: false,
        },
        restart_required: false,
        message: "The running Cartograph binary is current but could not be resolved safely for project reconciliation."
            .to_owned(),
        next_steps: vec![
            "Reinstall Cartograph through the native installer, then rerun `cartograph upgrade --apply --project-path <path>`."
                .to_owned(),
        ],
        registration_repair: None,
        project_reconciliation: None,
        registrations,
    }
}

async fn install_and_complete_upgrade(input: ResolvedUpgradeInput<'_>) -> UpgradeReport {
    let ResolvedUpgradeInput {
        current_text,
        latest_text,
        project_path,
        registrations,
        ..
    } = input;
    match apply_release(&latest_text).await {
        Ok(installed) => {
            complete_upgrade(CompletionInput {
                running_version: current_text,
                latest_version: latest_text.clone(),
                installed_version: latest_text,
                binary_applied: true,
                installed,
                project_path,
                registrations_before: registrations,
            })
            .await
        }
        Err(message) => UpgradeReport {
            status: "blocked",
            current_version: current_text.clone(),
            latest_version: Some(latest_text),
            installed_version: Some(current_text),
            operation: UpgradeOperationState {
                apply_requested: true,
                applied: false,
                completed: false,
            },
            restart_required: false,
            message,
            next_steps: vec![
                "Download the matching native asset and SHA256SUMS from the GitHub release."
                    .to_owned(),
                "Verify the checksum before replacing the current executable.".to_owned(),
            ],
            registration_repair: None,
            project_reconciliation: None,
            registrations,
        },
    }
}

async fn complete_upgrade(input: CompletionInput<'_>) -> UpgradeReport {
    let CompletionInput {
        running_version,
        latest_version,
        installed_version,
        binary_applied,
        installed,
        project_path,
        registrations_before,
    } = input;
    let InstalledBinary {
        path,
        launcher_warning,
    } = installed;
    let project_reconciliation =
        if project_upgrade_is_configured(project_path, &registrations_before) {
            Some(reconcile_project(&path, project_path, &installed_version).await)
        } else {
            None
        };
    let project_ready = project_reconciliation
        .as_ref()
        .is_none_or(ProjectReconciliation::upgrade_completed);
    let registration_repair = if project_ready {
        repair_stale_registrations(&path, project_path).await
    } else {
        RegistrationRepair::default()
    };
    let registrations = registration_audit(project_path, Some(&path));
    let launcher_blocked = launcher_warning.is_some();
    let blocked = launcher_blocked || !project_ready || registration_repair.failed > 0;
    let completed = !blocked;
    let host_configured = registrations
        .iter()
        .any(|registration| registration.cartograph_configured);
    let restart_required =
        host_restart_required(binary_applied, &registration_repair, host_configured);
    let next_steps = completion_next_steps(CompletionGuidance {
        launcher_warning: launcher_warning.as_deref(),
        project_reconciliation: project_reconciliation.as_ref(),
        registration_repair: &registration_repair,
        completed,
        host_configured,
        restart_required,
    });
    let status = completion_status(completed, binary_applied);
    let message = completion_message(completed, binary_applied, &installed_version);
    UpgradeReport {
        status,
        current_version: running_version,
        latest_version: Some(latest_version),
        installed_version: Some(installed_version),
        operation: UpgradeOperationState {
            apply_requested: true,
            applied: binary_applied,
            completed,
        },
        restart_required: completed && restart_required,
        message,
        next_steps,
        registration_repair: Some(registration_repair),
        project_reconciliation,
        registrations,
    }
}

const fn host_restart_required(
    binary_applied: bool,
    registration_repair: &RegistrationRepair,
    host_configured: bool,
) -> bool {
    host_configured && (binary_applied || registration_repair.repaired > 0)
}

fn completion_next_steps(input: CompletionGuidance<'_>) -> Vec<String> {
    let mut next_steps = Vec::with_capacity(6);
    if let Some(warning) = input.launcher_warning {
        next_steps.push(format!(
            "The release is installed, but a legacy PATH launcher could not be repointed ({warning}). Re-run `cartograph install` to repair project launchers."
        ));
    }
    add_project_reconciliation_steps(&mut next_steps, input.project_reconciliation);
    if input.registration_repair.failed > 0 {
        next_steps.push(
            "One or more stale MCP registrations could not be repinned automatically; apply only the manual steps reported for them, then restart that MCP host."
                .to_owned(),
        );
    } else if input.registration_repair.repaired > 0 {
        next_steps.push(format!(
            "Automatically repinned {} stale MCP registration(s) to the stable current launcher; each change is listed with its old and new executable, and wrapper commands, other arguments, and keys Cartograph does not own were preserved.",
            input.registration_repair.repaired
        ));
    }
    if input.completed && input.restart_required {
        next_steps.extend([
            "Close and reopen each configured MCP host once; an already-attached process cannot hot-reload the new binary."
                .to_owned(),
            "After reopening, verify the attached version, `tools/list`, `cartograph_status`, and one real query."
                .to_owned(),
        ]);
    } else if input.completed && input.host_configured {
        next_steps.push(
            "Configured MCP registrations already use the current launcher, and this invocation made no host-loaded change; no additional reopen is required. This does not prove the version of a process attached before an earlier upgrade."
                .to_owned(),
        );
    } else if input.completed {
        next_steps.push(
            "No MCP registration was found for this project; install one when an agent host should attach."
                .to_owned(),
        );
    }
    next_steps
}

const fn completion_status(completed: bool, binary_applied: bool) -> &'static str {
    if !completed {
        "blocked"
    } else if binary_applied {
        "updated"
    } else {
        "reconciled"
    }
}

fn completion_message(completed: bool, binary_applied: bool, installed_version: &str) -> String {
    if !completed {
        format!(
            "Cartograph {installed_version} is installed, but the complete project upgrade is blocked."
        )
    } else if binary_applied {
        format!(
            "Installed Cartograph {installed_version} and completed the safe project upgrade sequence."
        )
    } else {
        format!(
            "Cartograph {installed_version} was already installed; project state and owned registrations are reconciled."
        )
    }
}

fn project_upgrade_is_configured(
    project_path: &Path,
    registrations: &[InstallTargetDetection],
) -> bool {
    env::var_os(DATABASE_URL_ENV).is_some()
        || project_path.join(".cartograph").exists()
        || registrations
            .iter()
            .any(|registration| registration.cartograph_configured)
}

fn add_project_reconciliation_steps(
    next_steps: &mut Vec<String>,
    reconciliation: Option<&ProjectReconciliation>,
) {
    let Some(reconciliation) = reconciliation else {
        return;
    };
    if reconciliation.state == RECONCILED_FRESH {
        next_steps.push(
            "Database migrations, index refresh, doctor checks, and fresh next-process status all passed."
                .to_owned(),
        );
        return;
    }
    if reconciliation.state == RECONCILED_SOURCE_CHANGED {
        next_steps.push(
            "The upgrade is complete: the installed binary migrated the database, published or confirmed a complete current generation, and passed doctor and next-process status. The checkout changed after that generation was published, so the index is not fresh; run `cartograph index <path>` once edits pause, or let MCP auto-sync reconcile it. Rerunning the upgrade is not required."
                .to_owned(),
        );
        return;
    }
    if let Some(step) = index_retry_step(&reconciliation.index) {
        next_steps.push(step.to_owned());
        return;
    }
    if reconciliation.database.state == "timed_out" {
        next_steps.push(
            "The managed start timed out without concluding that the database is incompatible; rerun `cartograph upgrade --apply --project-path <path>` to resume the cold image pull or readiness wait."
                .to_owned(),
        );
        return;
    }
    if reconciliation.retryable {
        next_steps.push(
            "A verification step ended without a failure verdict (see `projectReconciliation.doctor` and `projectReconciliation.verification`); rerun `cartograph upgrade --apply --project-path <path>` to retry it."
                .to_owned(),
        );
        return;
    }
    if let Some(port) = reconciliation.managed_database_port
        && reconciliation.required_confirmation == Some("upgrade-managed-database")
    {
        next_steps.extend([
            format!(
                "Create a fresh private backup: `cartograph db backup ./cartograph-pre-upgrade.backup --project-path <path> --port {port}`."
            ),
            format!(
                "Then replace only the owned incompatible container: `cartograph db upgrade --project-path <path> --port {port} --confirm upgrade-managed-database`."
            ),
            "Rerun `cartograph upgrade --apply --project-path <path>`; the already-installed binary will resume the remaining safe steps."
                .to_owned(),
        ]);
    } else {
        next_steps.push(
            "Run the installed binary's `cartograph doctor <path>` for the exact failure, fix that boundary, then rerun `cartograph upgrade --apply --project-path <path>`."
                .to_owned(),
        );
    }
}

async fn reconcile_project(
    executable: &Path,
    project_path: &Path,
    installed_version: &str,
) -> ProjectReconciliation {
    let project_path = match fs::canonicalize(project_path) {
        Ok(path) if path.is_dir() => path,
        _ => return invalid_project_reconciliation(),
    };
    let database_mode = if env::var_os(DATABASE_URL_ENV).is_some() {
        ProjectDatabaseMode::External
    } else {
        match crate::resolve_managed_database_port(&project_path, None).await {
            Ok(port) => ProjectDatabaseMode::Managed(port),
            Err(_) => return unresolved_database_reconciliation(),
        }
    };
    reconcile_project_with(CompletionProjectInput {
        executable,
        project_path: &project_path,
        installed_version,
        database_mode,
        index_policy: DEFAULT_INDEX_CHILD_POLICY,
    })
    .await
}

struct CompletionProjectInput<'input> {
    executable: &'input Path,
    project_path: &'input Path,
    installed_version: &'input str,
    database_mode: ProjectDatabaseMode,
    index_policy: IndexChildPolicy,
}

impl CompletionProjectInput<'_> {
    const fn process(&self, command: ProjectCommand) -> ProjectProcessInput<'_> {
        ProjectProcessInput {
            executable: self.executable,
            project_path: self.project_path,
            database_mode: self.database_mode,
            command,
        }
    }
}

impl ProjectReconciliation {
    /// Whether the project part of the upgrade completed. Freshness is
    /// reported separately: a checkout edited after publication still
    /// completes the upgrade, as `source_changed`.
    fn upgrade_completed(&self) -> bool {
        self.state == RECONCILED_FRESH || self.state == RECONCILED_SOURCE_CHANGED
    }
}

async fn reconcile_project_with(input: CompletionProjectInput<'_>) -> ProjectReconciliation {
    let mut report = started_project_reconciliation(input.database_mode);
    if !reconcile_database(&input, &mut report).await {
        return report;
    }
    let Some(index) = reconcile_index(&input, &mut report).await else {
        return report;
    };
    if !reconcile_doctor(&input, &mut report).await {
        return report;
    }
    reconcile_status(&input, &mut report, &index).await;
    report
}

fn started_project_reconciliation(database_mode: ProjectDatabaseMode) -> ProjectReconciliation {
    ProjectReconciliation {
        state: "blocked",
        retryable: false,
        database: upgrade_step("not_run", "Database reconciliation did not run."),
        index: upgrade_step("not_run", "Index reconciliation did not run."),
        doctor: upgrade_step("not_run", "Doctor verification did not run."),
        verification: upgrade_step("not_run", "Freshness verification did not run."),
        fresh: false,
        generation_id: None,
        managed_database_port: match database_mode {
            ProjectDatabaseMode::External => None,
            ProjectDatabaseMode::Managed(port) => Some(port),
        },
        required_confirmation: None,
    }
}

async fn reconcile_database(
    input: &CompletionProjectInput<'_>,
    report: &mut ProjectReconciliation,
) -> bool {
    if input.database_mode == ProjectDatabaseMode::External {
        report.database = upgrade_step(
            "ready",
            "Using the validated external PostgreSQL boundary from the environment.",
        );
        return true;
    }
    let outcome = run_project_process(ProjectProcessInput {
        executable: input.executable,
        project_path: input.project_path,
        database_mode: input.database_mode,
        command: ProjectCommand::ManagedStart,
    })
    .await;
    match outcome {
        ProjectProcessOutcome::Succeeded => {
            report.database = upgrade_step(
                "ready",
                "The owned managed database is healthy and safe append-only migrations are current.",
            );
            return true;
        }
        ProjectProcessOutcome::TimedOut => {
            report.database = upgrade_step(
                "timed_out",
                "The managed database start exceeded its cold-image-pull and readiness budget; no compatibility conclusion was made.",
            );
            report.retryable = true;
            return false;
        }
        ProjectProcessOutcome::Failed => {}
    }
    report.database = upgrade_step(
        "blocked",
        "The owned managed database could not complete its idempotent start and migration step.",
    );
    if managed_database_upgrade_required(ProjectProcessInput {
        executable: input.executable,
        project_path: input.project_path,
        database_mode: input.database_mode,
        command: ProjectCommand::ManagedStatus,
    })
    .await
    {
        report.required_confirmation = Some("upgrade-managed-database");
    }
    false
}

/// Run the supervised index child; `Some` carries the generation it
/// published or confirmed so verification can prove what is current.
async fn reconcile_index(
    input: &CompletionProjectInput<'_>,
    report: &mut ProjectReconciliation,
) -> Option<IndexChildReport> {
    let command = configured_project_command(&input.process(ProjectCommand::Index));
    let outcome = index_child::run_index_child(command, input.index_policy).await;
    let (step, retryable) = index_step(&outcome);
    report.index = step;
    report.retryable = retryable;
    match outcome {
        IndexChildOutcome::Completed(index) => Some(index),
        IndexChildOutcome::Failed { .. } | IndexChildOutcome::TimedOut(_) => None,
    }
}

/// The reported index step and whether rerunning the same command is the
/// next action.
fn index_step(outcome: &IndexChildOutcome) -> (UpgradeStep, bool) {
    match outcome {
        IndexChildOutcome::Completed(index) if index.changed_after_publication => (
            upgrade_step(
                RECONCILED_SOURCE_CHANGED,
                "The new binary published a complete current generation, but the checkout changed again before the index finished; it reported that instead of rebuilding in a loop.",
            ),
            false,
        ),
        IndexChildOutcome::Completed(_) => (
            upgrade_step(
                "ready",
                "The new binary completed schema and current-generation reconciliation.",
            ),
            false,
        ),
        IndexChildOutcome::Failed { code } if code.as_deref() == Some(LEASE_BUSY_CODE) => (
            reasoned_step(
                ANOTHER_WRITER_ACTIVE,
                format!(
                    "Another Cartograph operation, such as an MCP server's auto-sync or a schema maintenance step, kept this project busy for the whole {}-minute bounded wait; this run published nothing.",
                    whole_minutes(crate::supervised_index::SUPERVISED_WRITER_WAIT)
                ),
                LEASE_BUSY_CODE,
            ),
            true,
        ),
        IndexChildOutcome::Failed { code } => (
            UpgradeStep {
                state: "blocked",
                message: "The new binary could not reconcile a complete current generation."
                    .to_owned(),
                reason: code.clone(),
            },
            false,
        ),
        IndexChildOutcome::TimedOut(timeout) => (
            reasoned_step(
                "timed_out",
                timed_out_message(*timeout),
                timeout.trigger.reason(),
            ),
            true,
        ),
    }
}

fn timed_out_message(timeout: IndexChildTimeout) -> String {
    let bound = match timeout.trigger {
        index_child::DeadlineTrigger::NoProgress => {
            let minutes = whole_minutes(index_child::INDEX_INACTIVITY_TIMEOUT);
            format!("reported no progress for {minutes} minutes")
        }
        index_child::DeadlineTrigger::Ceiling => format!(
            "reached its {}-minute absolute ceiling",
            whole_minutes(index_child::INDEX_ABSOLUTE_CEILING)
        ),
    };
    let stop = match timeout.stop {
        ChildStop::Cooperative => {
            "it stopped cooperatively and confirmed that it released its project lease and failed any unpublished staging generation".to_owned()
        }
        ChildStop::Unconfirmed => {
            "it exited after the cooperative stop request without confirming its cleanup, so its project lease may remain until its 5-minute TTL; a rerun waits for that".to_owned()
        }
        ChildStop::Forced => {
            let minutes = whole_minutes(index_child::INDEX_TERMINATION_GRACE);
            format!(
                "it did not exit within the {minutes}-minute cooperative stop grace and was killed, so its project lease expires on its own 5-minute TTL; a rerun waits for that"
            )
        }
    };
    format!(
        "The index {bound}; {stop}. Rerun the same upgrade command to resume, or run `cartograph index <path>` directly to see the stage that is not advancing."
    )
}

const fn whole_minutes(duration: Duration) -> u64 {
    duration.as_secs() / SECONDS_PER_MINUTE
}

const SECONDS_PER_MINUTE: u64 = 60;

/// The single next step when the index step ended in a retryable state.
fn index_retry_step(index: &UpgradeStep) -> Option<&'static str> {
    match index.state {
        ANOTHER_WRITER_ACTIVE => Some(
            "Another Cartograph operation kept this project busy; nothing is broken. Rerun `cartograph upgrade --apply --project-path <path>` after it finishes; the installed binary resumes the remaining steps.",
        ),
        "timed_out" => Some(
            "The index step was stopped at a bound without a failure verdict (see `projectReconciliation.index`). Rerun `cartograph upgrade --apply --project-path <path>` to resume; if it repeats, run `cartograph index <path>` directly to see the stage that is not advancing.",
        ),
        _ => None,
    }
}

async fn reconcile_doctor(
    input: &CompletionProjectInput<'_>,
    report: &mut ProjectReconciliation,
) -> bool {
    let (state, message) = match run_project_process(input.process(ProjectCommand::Doctor)).await {
        ProjectProcessOutcome::Succeeded => (
            "ready",
            "The new binary passed PostgreSQL, ParadeDB, pgvector, and project doctor checks.",
        ),
        ProjectProcessOutcome::Failed => (
            "blocked",
            "The new binary's capability and project doctor did not pass.",
        ),
        ProjectProcessOutcome::TimedOut => (
            "timed_out",
            "Doctor verification timed out; rerun the same upgrade command to retry it.",
        ),
    };
    report.doctor = upgrade_step(state, message);
    report.retryable = state == "timed_out";
    state == "ready"
}

async fn reconcile_status(
    input: &CompletionProjectInput<'_>,
    report: &mut ProjectReconciliation,
    index: &IndexChildReport,
) {
    let probe = run_status_probe(
        input.process(ProjectCommand::Status),
        input.installed_version,
    )
    .await;
    let verdict = match &probe {
        Ok(probe) => status_verdict(probe, index),
        Err(ProbeFailure::TimedOut) => StatusVerdict::TimedOut,
        Err(ProbeFailure::Failed) => StatusVerdict::Unproven,
    };
    let (state, message) = match verdict {
        StatusVerdict::Fresh => (
            RECONCILED_FRESH,
            "A next-process status reports the installed version and a fresh current generation.",
        ),
        StatusVerdict::SourceChanged => (
            RECONCILED_SOURCE_CHANGED,
            "A next-process status reports the installed version with the generation this upgrade published still current; only the checkout changed after publication.",
        ),
        StatusVerdict::Superseded => (
            "blocked",
            "Another writer replaced the generation this upgrade published and the checkout is not fresh; rerun the same upgrade command once that writer finishes.",
        ),
        StatusVerdict::Unproven => (
            "blocked",
            "The next-process version/freshness proof did not pass.",
        ),
        StatusVerdict::TimedOut => (
            "timed_out",
            "The next-process status exceeded its source-scan budget without a verdict; rerun the same upgrade command to retry it.",
        ),
    };
    report.verification = upgrade_step(state, message);
    report.retryable = matches!(verdict, StatusVerdict::Superseded | StatusVerdict::TimedOut);
    if let (Ok(probe), StatusVerdict::Fresh | StatusVerdict::SourceChanged) = (probe, verdict) {
        report.state = state;
        report.fresh = verdict == StatusVerdict::Fresh;
        report.generation_id = Some(probe.generation_id);
    }
}

/// What a next-process status proves about the generation the index step
/// published or confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusVerdict {
    /// Installed version with a fresh current generation.
    Fresh,
    /// That exact generation is still current and only the live checkout
    /// moved past its source revision.
    SourceChanged,
    /// A different generation became current and it is not fresh.
    Superseded,
    /// No version/generation proof.
    Unproven,
    /// The probe ran out of time before it could prove anything.
    TimedOut,
}

fn status_verdict(probe: &StatusProbe, index: &IndexChildReport) -> StatusVerdict {
    if probe.fresh {
        return StatusVerdict::Fresh;
    }
    if probe.generation_id != index.generation_id {
        return StatusVerdict::Superseded;
    }
    let ours = probe.current_source_revision == index.source_revision;
    let moved = probe
        .live_source_revision
        .as_deref()
        .is_some_and(|live| live != index.source_revision);
    if ours && moved {
        StatusVerdict::SourceChanged
    } else {
        StatusVerdict::Unproven
    }
}

fn invalid_project_reconciliation() -> ProjectReconciliation {
    blocked_project_reconciliation("The project path is not an existing real directory.", None)
}

fn unresolved_database_reconciliation() -> ProjectReconciliation {
    blocked_project_reconciliation(
        "The managed database port could not be resolved safely.",
        None,
    )
}

fn blocked_project_reconciliation(message: &str, port: Option<u16>) -> ProjectReconciliation {
    ProjectReconciliation {
        state: "blocked",
        retryable: false,
        database: upgrade_step("blocked", message),
        index: upgrade_step("not_run", "Index reconciliation did not run."),
        doctor: upgrade_step("not_run", "Doctor verification did not run."),
        verification: upgrade_step("not_run", "Freshness verification did not run."),
        fresh: false,
        generation_id: None,
        managed_database_port: port,
        required_confirmation: None,
    }
}

fn upgrade_step(state: &'static str, message: &str) -> UpgradeStep {
    UpgradeStep {
        state,
        message: message.to_owned(),
        reason: None,
    }
}

fn reasoned_step(state: &'static str, message: String, reason: &str) -> UpgradeStep {
    UpgradeStep {
        state,
        message,
        reason: Some(reason.to_owned()),
    }
}

async fn run_project_process(input: ProjectProcessInput<'_>) -> ProjectProcessOutcome {
    let timeout = input.command.timeout();
    let mut command = configured_project_command(&input);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    match tokio::time::timeout(timeout, command.status()).await {
        Ok(Ok(status)) if status.success() => ProjectProcessOutcome::Succeeded,
        Ok(_) => ProjectProcessOutcome::Failed,
        Err(_) => ProjectProcessOutcome::TimedOut,
    }
}

async fn managed_database_upgrade_required(input: ProjectProcessInput<'_>) -> bool {
    let Ok(value) = run_project_json(input).await else {
        return false;
    };
    managed_status_requires_upgrade(&value)
}

fn managed_status_requires_upgrade(value: &serde_json::Value) -> bool {
    let state = value.get("state").and_then(serde_json::Value::as_str);
    state != Some("missing")
        && (value
            .get("image_matches")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
            || value
                .get("hnsw_shared_memory_ready")
                .and_then(serde_json::Value::as_bool)
                == Some(false)
            || value
                .get("resource_limits_match")
                .and_then(serde_json::Value::as_bool)
                == Some(false))
}

/// Why a JSON probe produced no usable document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeFailure {
    /// The probe exceeded its command budget and was stopped.
    TimedOut,
    /// It failed to start, exited nonzero, overflowed, or was malformed.
    Failed,
}

async fn run_status_probe(
    input: ProjectProcessInput<'_>,
    installed_version: &str,
) -> Result<StatusProbe, ProbeFailure> {
    let value = run_project_json(input).await?;
    decode_status_probe(&value, installed_version).map_err(|()| ProbeFailure::Failed)
}

/// A next-process status that reports the installed version and a valid
/// current generation, with its freshness evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusProbe {
    generation_id: String,
    current_source_revision: String,
    live_source_revision: Option<String>,
    fresh: bool,
}

fn decode_status_probe(
    value: &serde_json::Value,
    installed_version: &str,
) -> Result<StatusProbe, ()> {
    if value.get("version").and_then(serde_json::Value::as_str) != Some(installed_version) {
        return Err(());
    }
    let text = |pointer| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let generation_id = text("/project/snapshot/current/generation_id")
        .and_then(|value| GenerationId::parse(&value).ok())
        .ok_or(())?;
    Ok(StatusProbe {
        generation_id: generation_id.as_str().to_owned(),
        current_source_revision: text("/project/snapshot/current/source_revision").ok_or(())?,
        live_source_revision: text("/project/live_source_revision"),
        fresh: value
            .pointer("/project/fresh")
            .and_then(serde_json::Value::as_bool)
            .ok_or(())?,
    })
}

async fn run_project_json(
    input: ProjectProcessInput<'_>,
) -> Result<serde_json::Value, ProbeFailure> {
    let budget = input.command.timeout();
    let mut command = configured_project_command(&input);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| ProbeFailure::Failed)?;
    let stdout = child.stdout.take().ok_or(ProbeFailure::Failed)?;
    let probe = async move {
        let mut bytes = Vec::with_capacity(MAXIMUM_STATUS_PROBE_BYTES.min(64 * 1024));
        stdout
            .take((MAXIMUM_STATUS_PROBE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| ProbeFailure::Failed)?;
        if bytes.len() > MAXIMUM_STATUS_PROBE_BYTES {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(ProbeFailure::Failed);
        }
        let status = child.wait().await.map_err(|_| ProbeFailure::Failed)?;
        Ok((status, bytes))
    };
    let (status, stdout) = tokio::time::timeout(budget, probe)
        .await
        .map_err(|_| ProbeFailure::TimedOut)??;
    if !status.success() {
        return Err(ProbeFailure::Failed);
    }
    serde_json::from_slice(&stdout).map_err(|_| ProbeFailure::Failed)
}

fn configured_project_command(input: &ProjectProcessInput<'_>) -> Command {
    let mut command = Command::new(input.executable);
    command
        .args(project_command_arguments(input))
        .current_dir(input.project_path)
        .kill_on_drop(true);
    if let ProjectDatabaseMode::Managed(port) = input.database_mode {
        command.env(crate::MANAGED_DATABASE_PORT_ENV, port.to_string());
    }
    command
}

fn project_command_arguments(input: &ProjectProcessInput<'_>) -> Vec<OsString> {
    let mut arguments = match input.command {
        ProjectCommand::ManagedStatus => vec![
            OsString::from("db"),
            OsString::from("status"),
            OsString::from("--project-path"),
            input.project_path.as_os_str().to_owned(),
        ],
        ProjectCommand::ManagedStart => vec![
            OsString::from("db"),
            OsString::from("start"),
            OsString::from("--project-path"),
            input.project_path.as_os_str().to_owned(),
        ],
        ProjectCommand::Index => vec![
            OsString::from("index"),
            input.project_path.as_os_str().to_owned(),
            OsString::from("--preserve-current-excludes"),
            OsString::from("--supervised"),
        ],
        ProjectCommand::Doctor => vec![
            OsString::from("doctor"),
            input.project_path.as_os_str().to_owned(),
        ],
        ProjectCommand::Status => vec![
            OsString::from("status"),
            input.project_path.as_os_str().to_owned(),
        ],
    };
    if matches!(
        input.command,
        ProjectCommand::ManagedStatus | ProjectCommand::ManagedStart
    ) && let ProjectDatabaseMode::Managed(port) = input.database_mode
    {
        arguments.push(OsString::from("--port"));
        arguments.push(OsString::from(port.to_string()));
    }
    arguments.push(OsString::from("--format"));
    arguments.push(OsString::from("json"));
    arguments
}

fn report_unknown(input: UnknownReportInput<'_>) -> UpgradeReport {
    let UnknownReportInput {
        current_version,
        apply,
        reason,
        registrations,
    } = input;
    UpgradeReport {
        status: "unknown",
        current_version: current_version.clone(),
        latest_version: None,
        installed_version: Some(current_version),
        operation: UpgradeOperationState {
            apply_requested: apply,
            applied: false,
            completed: false,
        },
        restart_required: false,
        message: format!("Could not resolve the latest Cartograph release: {reason}."),
        next_steps: vec![format!("Check {RELEASES_URL} manually.")],
        registration_repair: None,
        project_reconciliation: None,
        registrations,
    }
}

async fn repair_stale_registrations(executable: &Path, project_path: &Path) -> RegistrationRepair {
    let project_root =
        fs::canonicalize(project_path).unwrap_or_else(|_| project_path.to_path_buf());
    let home = host_home();
    repair_registrations(&RegistrationRepairInput {
        executable,
        scope: HostScope {
            project_root: &project_root,
            home: home.as_deref(),
            location: DiagnosticLocation::Both,
        },
    })
    .await
}

/// Repins every registration whose Cartograph executable is a stale absolute
/// path. A direct pin is reinstalled by the installed executable; a wrapped
/// registration is repinned in place, so only its embedded executable
/// argument changes and its wrapper, other arguments, and keys survive.
async fn repair_registrations(input: &RegistrationRepairInput<'_>) -> RegistrationRepair {
    let stale = detect_install_targets_in(&input.scope, Some(input.executable))
        .into_iter()
        .filter(InstallTargetDetection::needs_repin)
        .collect::<Vec<_>>();
    let pinned = install::registration_executable(input.executable)
        .unwrap_or_else(|| input.executable.to_string_lossy().into_owned());
    let mut report = RegistrationRepair {
        attempted: u16::try_from(stale.len()).unwrap_or(u16::MAX),
        ..RegistrationRepair::default()
    };
    for registration in &stale {
        let repinned = if registration.wrapped_executable_state.is_some() {
            repin_wrapped_registration(input, registration)
        } else {
            reinstall_direct_registration(input, registration).await
        };
        if repinned {
            report.repaired = report.repaired.saturating_add(1);
        } else {
            report.failed = report.failed.saturating_add(1);
        }
        report
            .changes
            .push(registration_change(registration, &pinned, repinned));
    }
    report
}

async fn reinstall_direct_registration(
    input: &RegistrationRepairInput<'_>,
    registration: &InstallTargetDetection,
) -> bool {
    let result = tokio::time::timeout(
        REGISTRATION_REPAIR_TIMEOUT,
        Command::new(input.executable)
            .args(registration_repair_args(registration))
            .arg(input.scope.project_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    matches!(result, Ok(Ok(status)) if status.success())
}

/// Rewrites only the wrapped entry through the installer's merge, which
/// replaces the embedded absolute executable argument and nothing else.
fn repin_wrapped_registration(
    input: &RegistrationRepairInput<'_>,
    registration: &InstallTargetDetection,
) -> bool {
    let Some(target) = InstallTarget::parse(registration.target) else {
        return false;
    };
    let location = if registration.location == "local" {
        InstallLocation::Local
    } else {
        InstallLocation::Global
    };
    let request = InstallRequest::new(&InstallRequestInput {
        project_root: input.scope.project_root,
        executable: input.executable,
        target,
        location,
        command_override: None,
        permissions: false,
    });
    let request = match input.scope.home {
        Some(home) => request.and_then(|request| request.with_home(home)),
        None => request,
    };
    request
        .and_then(|request| install::install_registration(&request))
        .is_ok_and(|report| report.changed())
}

fn registration_change(
    registration: &InstallTargetDetection,
    pinned: &str,
    repinned: bool,
) -> RegistrationChange {
    let executable = registration.executable.as_ref();
    let mut change = RegistrationChange {
        target: registration.target,
        location: registration.location,
        config_path: registration.config_path,
        command_state: registration.command_state,
        outcome: if repinned {
            REPAIR_REPINNED
        } else {
            REPAIR_MANUAL
        },
        field: executable
            .and_then(|executable| executable.argument_index)
            .map_or_else(
                || COMMAND_FIELD.to_owned(),
                |index| format!("args[{index}]"),
            ),
        wrapper: executable.and_then(|executable| executable.wrapper.clone()),
        from: executable
            .map(|executable| executable.path.clone())
            .unwrap_or_default(),
        to: pinned.to_owned(),
        manual_step: None,
    };
    if !repinned {
        change.manual_step = Some(manual_repin_step(
            &change,
            registration.repin_command.as_deref(),
        ));
    }
    change
}

fn manual_repin_step(change: &RegistrationChange, repin_command: Option<&str>) -> String {
    match (change.wrapper.as_deref(), repin_command) {
        (None, Some(command)) => format!("Run `{command}`, then restart that MCP host."),
        _ => format!(
            "Edit {}: in the `cartograph` entry replace {} `{}` with `{}`, keep the command, the other arguments, and every other key unchanged, then restart that MCP host.",
            change.config_path, change.field, change.from, change.to
        ),
    }
}

fn registration_repair_args(registration: &InstallTargetDetection) -> Vec<String> {
    let mut arguments = vec![
        "install".to_owned(),
        "--yes".to_owned(),
        "--no-permissions".to_owned(),
        "--no-hooks".to_owned(),
        "--target".to_owned(),
        registration.target.to_owned(),
        "--location".to_owned(),
        registration.location.to_owned(),
    ];
    if let Some(port) = registration.managed_database_port {
        arguments.push("--managed-database-port".to_owned());
        arguments.push(port.to_string());
    }
    arguments.push("--project-path".to_owned());
    arguments
}

fn registration_audit(
    project_path: &Path,
    selected_executable: Option<&Path>,
) -> Vec<InstallTargetDetection> {
    let project_path =
        fs::canonicalize(project_path).unwrap_or_else(|_| project_path.to_path_buf());
    detect_install_targets(&project_path, DiagnosticLocation::Both, selected_executable)
}

fn registration_next_steps(registrations: &[InstallTargetDetection]) -> Vec<String> {
    if registrations
        .iter()
        .any(InstallTargetDetection::needs_repin)
    {
        vec!["Run each reported repin command, then restart that MCP host.".to_owned()]
    } else if registrations
        .iter()
        .any(|registration| registration.cartograph_configured)
    {
        vec![
            "No native update action is needed; configured MCP registrations are audited below."
                .to_owned(),
        ]
    } else {
        vec!["No configured MCP registrations were discovered for this project.".to_owned()]
    }
}

async fn latest_version() -> Result<String, String> {
    match latest_git_tag().await {
        Ok(version) => Ok(version),
        Err(git_error) => latest_release_api().await.map_err(|api_error| {
            format!("git tag lookup failed ({git_error}); GitHub API failed ({api_error})")
        }),
    }
}

async fn latest_git_tag() -> Result<String, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("git")
            .args(["ls-remote", "--tags", "--refs", REMOTE, "v*"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "git ls-remote timed out".to_owned())?
    .map_err(|_| "git ls-remote could not start".to_owned())?;
    if !output.status.success() || output.stdout.len() > MAXIMUM_TAG_OUTPUT_BYTES {
        return Err("git ls-remote failed or exceeded its output bound".to_owned());
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "git ls-remote returned non-UTF-8 output".to_owned())?;
    text.lines()
        .filter_map(|line| line.split_once("refs/tags/v").map(|(_, version)| version))
        .filter_map(|version| parse_version(version).map(|parsed| (parsed, version.to_owned())))
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, version)| version)
        .ok_or_else(|| "git ls-remote returned no semver release tags".to_owned())
}

async fn latest_release_api() -> Result<String, String> {
    let client = http_client()?;
    let bytes = fetch_bounded(&client, LATEST_RELEASE_API, MAXIMUM_CHECKSUM_BYTES).await?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| "GitHub API returned invalid JSON".to_owned())?;
    let tag = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .and_then(|tag| tag.strip_prefix('v'))
        .ok_or_else(|| "GitHub API response has no release tag".to_owned())?;
    parse_version(tag)
        .map(|_| tag.to_owned())
        .ok_or_else(|| "GitHub release tag is not semver".to_owned())
}

async fn apply_release(version: &str) -> Result<InstalledBinary, String> {
    let executable =
        env::current_exe().map_err(|_| "could not resolve the running executable".to_owned())?;
    let executable = fs::canonicalize(&executable)
        .map_err(|_| "could not resolve the running executable".to_owned())?;
    let asset = ASSET_NAME.map_err(str::to_owned)?;
    let client = http_client()?;
    let checksums_url = format!("{RELEASE_BASE}/v{version}/SHA256SUMS");
    let asset_url = format!("{RELEASE_BASE}/v{version}/{asset}");
    let checksums = fetch_bounded(&client, &checksums_url, MAXIMUM_CHECKSUM_BYTES).await?;
    let expected = checksum_for_asset(&checksums, asset)?;
    let binary = fetch_bounded(&client, &asset_url, MAXIMUM_BINARY_BYTES).await?;
    let actual = sha256_hex(&binary);
    if actual != expected {
        return Err("downloaded native binary failed SHA-256 verification".to_owned());
    }
    install_binary(&executable, &binary, version)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(char::from(
            LOWER_HEX_DIGITS[usize::from(byte >> HIGH_NIBBLE_SHIFT)],
        ));
        output.push(char::from(
            LOWER_HEX_DIGITS[usize::from(byte & LOW_NIBBLE_MASK)],
        ));
    }
    output
}

fn http_client() -> Result<Client, String> {
    cartograph_llm::ensure_tls_crypto_provider()
        .map_err(|_| "could not initialize the HTTPS client".to_owned())?;
    Client::builder()
        .user_agent(format!("cartograph/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_mins(2))
        .build()
        .map_err(|_| "could not initialize the HTTPS client".to_owned())
}

async fn fetch_bounded(client: &Client, url: &str, maximum: usize) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "release download failed".to_owned())?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > maximum as u64)
    {
        return Err("release download was unavailable or exceeded its size bound".to_owned());
    }
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "release download failed".to_owned())?;
        let next = output
            .len()
            .checked_add(chunk.len())
            .filter(|length| *length <= maximum)
            .ok_or_else(|| "release download exceeded its size bound".to_owned())?;
        output
            .try_reserve(next.saturating_sub(output.len()))
            .map_err(|_| "release download exceeded local memory limits".to_owned())?;
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn checksum_for_asset(checksums: &[u8], asset: &str) -> Result<String, String> {
    let text = std::str::from_utf8(checksums).map_err(|_| "SHA256SUMS is not UTF-8".to_owned())?;
    let matches = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            (name == asset && fields.next().is_none()).then_some(digest)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1
        || matches[0].len() != 64
        || !matches[0].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("SHA256SUMS has no unique valid entry for this platform".to_owned());
    }
    Ok(matches[0].to_ascii_lowercase())
}

fn install_binary(
    executable: &Path,
    bytes: &[u8],
    version: &str,
) -> Result<InstalledBinary, String> {
    #[cfg(unix)]
    {
        let launcher_path = env::var_os("PATH");
        install_binary_unix(&InstallBinaryInput {
            executable,
            bytes,
            version,
            launcher_path: launcher_path.as_deref(),
        })
    }

    #[cfg(not(unix))]
    {
        install_binary_in_place(executable, bytes, version)?;
        Ok(InstalledBinary {
            path: executable.to_path_buf(),
            launcher_warning: None,
        })
    }
}

#[cfg(unix)]
struct InstallBinaryInput<'input> {
    executable: &'input Path,
    bytes: &'input [u8],
    version: &'input str,
    launcher_path: Option<&'input std::ffi::OsStr>,
}

#[cfg(unix)]
fn install_binary_unix(input: &InstallBinaryInput<'_>) -> Result<InstalledBinary, String> {
    if let Some(layout) = VersionedInstallLayout::detect(input.executable, input.version)? {
        return layout.install(input);
    }

    let &InstallBinaryInput {
        executable,
        bytes,
        version,
        ..
    } = input;
    install_binary_in_place(executable, bytes, version)?;
    Ok(InstalledBinary {
        path: executable.to_path_buf(),
        launcher_warning: None,
    })
}

fn install_binary_in_place(executable: &Path, bytes: &[u8], version: &str) -> Result<(), String> {
    let directory = executable
        .parent()
        .ok_or_else(|| "running executable has no parent directory".to_owned())?;
    let mut staged = NamedTempFile::new_in(directory)
        .map_err(|_| "could not stage the native update beside the executable".to_owned())?;
    staged
        .write_all(bytes)
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|_| "could not write the staged native update".to_owned())?;
    #[cfg(unix)]
    set_executable(staged.as_file())?;
    let staged = staged.into_temp_path();
    verify_staged_binary(staged.as_ref(), version)?;
    replace_executable(staged, executable)
}

#[cfg(unix)]
struct VersionedInstallLayout {
    install_root: PathBuf,
    release_name: String,
    release_root: PathBuf,
    executable: PathBuf,
}

#[cfg(unix)]
impl VersionedInstallLayout {
    fn detect(executable: &Path, version: &str) -> Result<Option<Self>, String> {
        let Some(bin_directory) = executable.parent() else {
            return Ok(None);
        };
        let Some(previous_release) = bin_directory.parent() else {
            return Ok(None);
        };
        let Some(versions_directory) = previous_release.parent() else {
            return Ok(None);
        };
        let Some(install_root) = versions_directory.parent() else {
            return Ok(None);
        };
        let is_versioned_layout = bin_directory.file_name().is_some_and(|name| name == "bin")
            && versions_directory
                .file_name()
                .is_some_and(|name| name == "versions")
            && previous_release
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.strip_prefix('v').and_then(parse_version).is_some());
        if !is_versioned_layout {
            return Ok(None);
        }
        let release_name = version_release_directory(version)?;
        let release_root = versions_directory.join(&release_name);
        let executable = release_root.join("bin").join(
            executable
                .file_name()
                .ok_or_else(|| "running executable has no file name".to_owned())?,
        );
        Ok(Some(Self {
            install_root: install_root.to_path_buf(),
            release_name,
            release_root,
            executable,
        }))
    }

    fn install(self, input: &InstallBinaryInput<'_>) -> Result<InstalledBinary, String> {
        let &InstallBinaryInput {
            executable: prior_executable,
            bytes,
            version,
            launcher_path,
        } = input;
        ensure_real_directory(&self.release_root)?;
        let bin_directory = self
            .executable
            .parent()
            .ok_or_else(|| "versioned executable has no parent directory".to_owned())?;
        ensure_real_directory(bin_directory)?;
        install_binary_in_place(&self.executable, bytes, version)?;

        let current_target = Path::new("versions").join(&self.release_name);
        atomic_repoint_symlink(
            &self.install_root.join("current"),
            &current_target,
            "current release",
        )?;
        let launcher_target = self.install_root.join("current/bin/cartograph");
        let launcher_warning = launcher_path.and_then(|path| {
            repoint_matching_path_launchers_in(path, prior_executable, &launcher_target).err()
        });
        Ok(InstalledBinary {
            path: self.executable,
            launcher_warning,
        })
    }
}

#[cfg(unix)]
fn version_release_directory(version: &str) -> Result<String, String> {
    let version = version.strip_prefix('v').unwrap_or(version);
    if version.is_empty()
        || version.len() > 128
        || parse_version(version).is_none()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err("published version is not a safe install directory name".to_owned());
    }
    Ok(format!("v{version}"))
}

#[cfg(unix)]
fn ensure_real_directory(directory: &Path) -> Result<(), String> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err("versioned update destination is not a real directory".to_owned());
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err("could not inspect the versioned update destination".to_owned());
        }
        Err(_) => {}
    }
    fs::create_dir_all(directory)
        .map_err(|_| "could not create the versioned update destination".to_owned())?;
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_| "could not inspect the versioned update destination".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("versioned update destination is not a real directory".to_owned());
    }
    Ok(())
}

#[cfg(unix)]
fn repoint_matching_path_launchers_in(
    path: &std::ffi::OsStr,
    prior_executable: &Path,
    launcher_target: &Path,
) -> Result<(), String> {
    const MAXIMUM_PATH_DIRECTORIES: usize = 128;
    let prior_executable = fs::canonicalize(prior_executable)
        .map_err(|_| "could not resolve the prior Cartograph executable".to_owned())?;
    for directory in env::split_paths(&path).take(MAXIMUM_PATH_DIRECTORIES) {
        let launcher = directory.join("cartograph");
        let Ok(metadata) = fs::symlink_metadata(&launcher) else {
            continue;
        };
        if !metadata.file_type().is_symlink()
            || fs::canonicalize(&launcher).ok().as_deref() != Some(prior_executable.as_path())
        {
            continue;
        }
        atomic_repoint_symlink(&launcher, launcher_target, "Cartograph launcher")?;
    }
    Ok(())
}

#[cfg(unix)]
fn atomic_repoint_symlink(link: &Path, target: &Path, label: &str) -> Result<(), String> {
    use std::os::unix::fs::symlink;

    if let Ok(metadata) = fs::symlink_metadata(link)
        && !metadata.file_type().is_symlink()
    {
        return Err(format!("{label} path is not a symbolic link"));
    }
    let parent = link
        .parent()
        .ok_or_else(|| format!("{label} path has no parent directory"))?;
    let staging = tempfile::Builder::new()
        .prefix(".cartograph-link-")
        .tempdir_in(parent)
        .map_err(|_| format!("could not stage the {label} link"))?;
    let staged_link = staging.path().join("link");
    symlink(target, &staged_link).map_err(|_| format!("could not stage the {label} link"))?;
    fs::rename(&staged_link, link).map_err(|_| format!("could not replace the {label} link"))
}

struct StagedBinaryOutput {
    success: bool,
    stdout: Vec<u8>,
}

fn verify_staged_binary(path: &Path, version: &str) -> Result<(), String> {
    verify_staged_binary_with(version, || {
        ProcessCommand::new(path)
            .arg("--version")
            .output()
            .map(|output| StagedBinaryOutput {
                success: output.status.success(),
                stdout: output.stdout,
            })
    })
}

fn verify_staged_binary_with(
    version: &str,
    mut launch: impl FnMut() -> std::io::Result<StagedBinaryOutput>,
) -> Result<(), String> {
    let mut attempts = 0;
    let output = loop {
        attempts += 1;
        match launch() {
            Ok(output) => break output,
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempts < MAXIMUM_STAGED_BINARY_LAUNCH_ATTEMPTS =>
            {
                std::thread::sleep(STAGED_BINARY_LAUNCH_RETRY_DELAY);
            }
            Err(_) => return Err("downloaded native binary could not start".to_owned()),
        }
    };
    let stdout = std::str::from_utf8(&output.stdout).unwrap_or_default();
    if output.success && stdout.split_whitespace().any(|part| part == version) {
        Ok(())
    } else {
        Err("downloaded native binary did not report the expected version".to_owned())
    }
}

#[cfg(unix)]
fn set_executable(file: &fs::File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .map_err(|_| "could not mark the native update executable".to_owned())
}

#[cfg(unix)]
fn replace_executable(staged: TempPath, executable: &Path) -> Result<(), String> {
    staged
        .persist(executable)
        .map_err(|_| "could not atomically replace the running executable".to_owned())
}

#[cfg(windows)]
fn replace_executable(staged: TempPath, executable: &Path) -> Result<(), String> {
    let backup = executable.with_extension("exe.cartograph-old");
    let _ = fs::remove_file(&backup);
    fs::rename(executable, &backup)
        .map_err(|_| "could not move the running executable aside".to_owned())?;
    staged.persist(executable).map_err(|_| {
        let _ = fs::rename(&backup, executable);
        "could not replace the running executable; the prior binary was restored".to_owned()
    })
}

const ASSET_NAME: Result<&str, &str> = cfg_select! {
    all(target_os = "macos", target_arch = "aarch64") => Ok("cartograph-darwin-arm64"),
    all(target_os = "macos", target_arch = "x86_64") => {
        Err("Intel macOS is not supported; use Apple Silicon with macOS 26 or newer")
    }
    all(target_os = "linux", target_arch = "aarch64") => Ok("cartograph-linux-arm64"),
    all(target_os = "linux", target_arch = "x86_64") => Ok("cartograph-linux-x64"),
    all(target_os = "windows", target_arch = "x86_64") => Ok("cartograph-windows-x64.exe"),
    _ => Err("no native release asset exists for this operating system and architecture"),
};

fn parse_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let raw = raw.split_once('+').map_or(raw, |(core, _)| core);
    let (core, prerelease) = raw.split_once('-').unwrap_or((raw, ""));
    let mut numbers = core.split('.');
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers.next()?.parse().ok()?;
    let patch = numbers.next()?.parse().ok()?;
    if numbers.next().is_some() {
        return None;
    }
    let prerelease = if prerelease.is_empty() {
        Vec::new()
    } else {
        prerelease
            .split('.')
            .map(|part| {
                part.parse::<u64>().map_or_else(
                    |_| PrereleasePart::Text(part.to_ascii_lowercase()),
                    PrereleasePart::Numeric,
                )
            })
            .collect()
    };
    Some(Version {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn compare_prerelease(left: &[PrereleasePart], right: &[PrereleasePart]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left, right) in left.iter().zip(right) {
        let ordering = match (left, right) {
            (PrereleasePart::Numeric(left), PrereleasePart::Numeric(right)) => left.cmp(right),
            (PrereleasePart::Numeric(_), PrereleasePart::Text(_)) => Ordering::Less,
            (PrereleasePart::Text(_), PrereleasePart::Numeric(_)) => Ordering::Greater,
            (PrereleasePart::Text(left), PrereleasePart::Text(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use std::{io::Read as _, net::TcpListener, thread, time::Duration};

    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const FIXTURE_REQUEST_BYTES: usize = 8 * 1_024;

    #[test]
    fn semver_ordering_handles_stable_and_prerelease_tags() {
        let parse = |value| {
            parse_version(value).unwrap_or_else(|| panic!("invalid version fixture: {value}"))
        };
        assert!(parse("2.0.0") > parse("2.0.0-rc.2"));
        assert!(parse("2.0.0-rc.10") > parse("2.0.0-rc.2"));
        assert!(parse("2.0.0-alpha.1") > parse("1.1.33"));
        assert!(parse("2.1.0") > parse("2.0.99"));
    }

    #[test]
    fn checksum_parser_requires_one_exact_asset_entry() {
        let sums = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  cartograph-darwin-arm64\n";
        assert_eq!(
            checksum_for_asset(sums, "cartograph-darwin-arm64"),
            Ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned())
        );
        assert!(checksum_for_asset(sums, "cartograph-linux-x64").is_err());
        let duplicate = [sums.as_slice(), sums.as_slice()].concat();
        assert!(checksum_for_asset(&duplicate, "cartograph-darwin-arm64").is_err());
        assert!(checksum_for_asset(b"not utf8: \xff", "cartograph-darwin-arm64").is_err());
        assert_eq!(
            sha256_hex(b"cartograph"),
            "122eee0b90506d8a158a312f2f45814f50cb70d98668ba27289764d98af85141"
        );
    }

    #[test]
    fn report_and_semver_failures_remain_actionable_without_claiming_success() {
        let report = report_unknown(UnknownReportInput {
            current_version: "2.0.0".to_owned(),
            apply: true,
            reason: "fixture lookup failed",
            registrations: Vec::new(),
        });
        assert!(!succeeded(&report));
        let rendered = render(&report);
        assert!(rendered.contains("fixture lookup failed"));
        assert!(rendered.contains(RELEASES_URL));

        for invalid in ["", "2", "2.0", "2.0.0.1", "two.0.0"] {
            assert!(parse_version(invalid).is_none(), "accepted {invalid}");
        }
        let parse = |value| {
            parse_version(value).unwrap_or_else(|| panic!("invalid version fixture: {value}"))
        };
        assert_eq!(parse("v2.0.0+build.7"), parse("2.0.0"));
        assert!(parse("2.0.0-alpha") > parse("2.0.0-1"));
        assert!(parse("2.0.0-alpha.2") > parse("2.0.0-alpha.1"));
        assert!(parse("2.0.0-alpha.1") > parse("2.0.0-alpha"));
        assert!(ASSET_NAME.is_ok());
    }

    #[test]
    fn registration_audit_rendering_keeps_stale_repins_explicit() {
        let registration = InstallTargetDetection {
            target: "codex",
            location: "local",
            config_present: true,
            config_valid: true,
            cartograph_configured: true,
            config_path: ".codex/config.toml",
            command_state: "stale_absolute",
            wrapped_executable_state: None,
            managed_database_port: None,
            repin_command: Some(
                "cartograph install --yes --target codex --location local --project-path <path>"
                    .to_owned(),
            ),
            executable: None,
        };
        let wrapped = InstallTargetDetection {
            target: "cursor",
            location: "local",
            config_path: ".cursor/mcp.json",
            command_state: "wrapped",
            wrapped_executable_state: Some("stale_absolute"),
            repin_command: None,
            ..registration.clone()
        };
        assert!(wrapped.needs_repin());
        assert!(
            registration_next_steps(std::slice::from_ref(&wrapped))[0]
                .contains("Run each reported repin command")
        );
        let current_wrapper = InstallTargetDetection {
            wrapped_executable_state: Some("current_absolute"),
            ..wrapped.clone()
        };
        assert!(!current_wrapper.needs_repin());
        assert!(registration_next_steps(&[current_wrapper])[0].contains("No native update action"));
        let report = UpgradeReport {
            status: "current",
            current_version: "2.1.0".to_owned(),
            latest_version: Some("2.1.0".to_owned()),
            installed_version: Some("2.1.0".to_owned()),
            operation: UpgradeOperationState {
                apply_requested: false,
                applied: false,
                completed: true,
            },
            restart_required: false,
            message: "Cartograph 2.1.0 is current.".to_owned(),
            next_steps: registration_next_steps(std::slice::from_ref(&registration)),
            registration_repair: None,
            project_reconciliation: None,
            registrations: vec![registration, wrapped],
        };
        let rendered = render(&report);
        assert!(rendered.contains("MCP registration audit"));
        assert!(rendered.contains("stale_absolute"));
        assert!(rendered.contains("cursor local: wrapped (wrapped executable: stale_absolute)"));
        assert!(rendered.contains("Repin: cartograph install"));
        assert!(rendered.contains("restart that MCP host"));
        assert!(registration_next_steps(&[])[0].contains("No configured MCP registrations"));
    }

    #[test]
    fn upgrade_report_distinguishes_running_published_and_installed_versions()
    -> Result<(), serde_json::Error> {
        let report = UpgradeReport {
            status: "reconciled",
            current_version: "2.1.7-rc.1".to_owned(),
            latest_version: Some("2.1.6".to_owned()),
            installed_version: Some("2.1.7-rc.1".to_owned()),
            operation: UpgradeOperationState {
                apply_requested: true,
                applied: false,
                completed: true,
            },
            restart_required: false,
            message: "fixture reconciliation complete".to_owned(),
            next_steps: Vec::new(),
            registration_repair: Some(RegistrationRepair::default()),
            project_reconciliation: None,
            registrations: Vec::new(),
        };
        let encoded = serde_json::to_value(&report)?;
        assert_eq!(encoded["currentVersion"], "2.1.7-rc.1");
        assert_eq!(encoded["latestVersion"], "2.1.6");
        assert_eq!(encoded["installedVersion"], "2.1.7-rc.1");
        assert_eq!(encoded["completed"], true);
        assert_eq!(encoded["restartRequired"], false);
        Ok(())
    }

    #[test]
    fn host_restart_is_required_only_for_a_change_loaded_by_the_host() {
        let unchanged = RegistrationRepair::default();
        let repaired = RegistrationRepair {
            attempted: 1,
            repaired: 1,
            failed: 0,
            changes: Vec::new(),
        };
        assert!(!host_restart_required(false, &unchanged, true));
        assert!(host_restart_required(true, &unchanged, true));
        assert!(host_restart_required(false, &repaired, true));
        assert!(!host_restart_required(true, &unchanged, false));
    }

    #[test]
    fn idempotent_registered_upgrade_does_not_request_another_reopen() {
        let steps = completion_next_steps(CompletionGuidance {
            launcher_warning: None,
            project_reconciliation: None,
            registration_repair: &RegistrationRepair::default(),
            completed: true,
            host_configured: true,
            restart_required: false,
        });
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("no additional reopen is required"));
        assert!(!steps[0].contains("No MCP registration was found"));
    }

    #[test]
    fn managed_start_has_a_cold_pull_budget_distinct_from_short_probes() {
        assert_eq!(
            ProjectCommand::ManagedStart.timeout(),
            Duration::from_mins(15)
        );
        assert_eq!(
            ProjectCommand::ManagedStatus.timeout(),
            Duration::from_mins(2)
        );
        // Doctor and status each rescan the checkout, so they get a
        // source-scan budget rather than the database probe's.
        assert_eq!(ProjectCommand::Doctor.timeout(), Duration::from_mins(10));
        assert_eq!(ProjectCommand::Status.timeout(), Duration::from_mins(10));
    }

    #[test]
    fn verification_timeouts_stay_retryable_and_distinct_from_failures() {
        let mut report = started_project_reconciliation(ProjectDatabaseMode::External);
        report.doctor = upgrade_step("timed_out", "fixture doctor timeout");
        report.retryable = true;
        let mut steps = Vec::new();
        add_project_reconciliation_steps(&mut steps, Some(&report));
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("rerun"));
        assert!(!steps[0].contains("doctor <path>` for the exact failure"));

        report.retryable = false;
        report.doctor = upgrade_step("blocked", "fixture doctor failure");
        let mut steps = Vec::new();
        add_project_reconciliation_steps(&mut steps, Some(&report));
        assert!(steps[0].contains("for the exact failure"));
    }

    #[test]
    fn index_bounds_fit_large_projects_without_becoming_unbounded() {
        // A 26-minute full index on a large project must fit, and so must a
        // competing writer's wait followed by a full two-hour build.
        assert_eq!(
            index_child::INDEX_ABSOLUTE_CEILING,
            Duration::from_mins(180)
        );
        assert_eq!(ProjectCommand::Index.timeout(), Duration::from_mins(180));
        assert!(
            index_child::INDEX_ABSOLUTE_CEILING
                >= crate::supervised_index::SUPERVISED_WRITER_WAIT + Duration::from_hours(2)
        );
        // The backstop leaves the child's own 10-minute stall detector to
        // report the precise stage first.
        assert_eq!(
            index_child::INDEX_INACTIVITY_TIMEOUT,
            Duration::from_mins(15)
        );
        // A forced stop never leaves a lease longer than the 5-minute TTL
        // the old immediate kill did.
        assert!(index_child::INDEX_TERMINATION_GRACE < Duration::from_mins(5));
    }

    #[test]
    fn project_command_arguments_are_explicit_and_machine_readable() {
        let project_path = Path::new("/fixture/project");
        let managed_start = project_command_arguments(&ProjectProcessInput {
            executable: Path::new("/fixture/cartograph"),
            project_path,
            database_mode: ProjectDatabaseMode::Managed(55_433),
            command: ProjectCommand::ManagedStart,
        });
        assert_eq!(
            managed_start,
            [
                "db",
                "start",
                "--project-path",
                "/fixture/project",
                "--port",
                "55433",
                "--format",
                "json",
            ]
            .map(OsString::from)
        );

        assert_eq!(
            project_command_arguments(&ProjectProcessInput {
                executable: Path::new("/fixture/cartograph"),
                project_path,
                database_mode: ProjectDatabaseMode::External,
                command: ProjectCommand::Index,
            }),
            [
                "index",
                "/fixture/project",
                "--preserve-current-excludes",
                "--supervised",
                "--format",
                "json",
            ]
            .map(OsString::from)
        );

        for (command, first) in [
            (ProjectCommand::Doctor, "doctor"),
            (ProjectCommand::Status, "status"),
        ] {
            assert_eq!(
                project_command_arguments(&ProjectProcessInput {
                    executable: Path::new("/fixture/cartograph"),
                    project_path,
                    database_mode: ProjectDatabaseMode::External,
                    command,
                }),
                [first, "/fixture/project", "--format", "json"].map(OsString::from)
            );
        }
    }

    #[test]
    fn managed_replacement_requires_positive_incompatibility_evidence() {
        assert!(!managed_status_requires_upgrade(&serde_json::json!({
            "state": "missing",
            "image_matches": false,
            "hnsw_shared_memory_ready": false,
            "resource_limits_match": false
        })));
        assert!(!managed_status_requires_upgrade(&serde_json::json!({
            "state": "healthy",
            "image_matches": true,
            "hnsw_shared_memory_ready": true,
            "resource_limits_match": true
        })));
        assert!(managed_status_requires_upgrade(&serde_json::json!({
            "state": "stopped",
            "image_matches": false,
            "hnsw_shared_memory_ready": true,
            "resource_limits_match": true
        })));
        assert!(managed_status_requires_upgrade(&serde_json::json!({
            "state": "healthy",
            "image_matches": true,
            "hnsw_shared_memory_ready": false,
            "resource_limits_match": true
        })));
        assert!(managed_status_requires_upgrade(&serde_json::json!({
            "state": "healthy",
            "image_matches": true,
            "hnsw_shared_memory_ready": true,
            "resource_limits_match": false
        })));
        assert!(!managed_status_requires_upgrade(&serde_json::json!({
            "state": "healthy"
        })));
    }

    const FIXTURE_GENERATION: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER_GENERATION: &str = "22222222-2222-4222-8222-222222222222";

    fn fixture_status(fresh: bool, live_source_revision: &str) -> serde_json::Value {
        serde_json::json!({
            "version": "2.1.7",
            "project": {
                "fresh": fresh,
                "live_source_revision": live_source_revision,
                "snapshot": {"current": {
                    "generation_id": FIXTURE_GENERATION,
                    "source_revision": "published-revision"
                }}
            }
        })
    }

    fn fixture_index_report() -> IndexChildReport {
        IndexChildReport {
            generation_id: FIXTURE_GENERATION.to_owned(),
            source_revision: "published-revision".to_owned(),
            changed_after_publication: true,
        }
    }

    #[test]
    fn status_probe_requires_the_installed_version_and_a_valid_current_generation() {
        let status = fixture_status(true, "published-revision");
        assert_eq!(
            decode_status_probe(&status, "2.1.7"),
            Ok(StatusProbe {
                generation_id: FIXTURE_GENERATION.to_owned(),
                current_source_revision: "published-revision".to_owned(),
                live_source_revision: Some("published-revision".to_owned()),
                fresh: true,
            })
        );

        let mut wrong_version = status.clone();
        wrong_version["version"] = serde_json::json!("2.1.6");
        assert_eq!(decode_status_probe(&wrong_version, "2.1.7"), Err(()));
        let mut missing_freshness = status.clone();
        missing_freshness["project"]["fresh"] = serde_json::Value::Null;
        assert_eq!(decode_status_probe(&missing_freshness, "2.1.7"), Err(()));
        let mut missing_generation = status.clone();
        missing_generation["project"]["snapshot"]["current"] = serde_json::Value::Null;
        assert_eq!(decode_status_probe(&missing_generation, "2.1.7"), Err(()));
        let mut malformed_generation = status;
        malformed_generation["project"]["snapshot"]["current"]["generation_id"] =
            serde_json::json!("not-a-generation");
        assert_eq!(decode_status_probe(&malformed_generation, "2.1.7"), Err(()));
    }

    #[test]
    fn stale_status_completes_only_with_proof_that_only_the_checkout_moved() {
        let decode = |value: &serde_json::Value| {
            decode_status_probe(value, "2.1.7")
                .unwrap_or_else(|()| panic!("fixture status did not decode: {value}"))
        };
        let index = fixture_index_report();
        assert_eq!(
            status_verdict(&decode(&fixture_status(true, "published-revision")), &index),
            StatusVerdict::Fresh
        );
        assert_eq!(
            status_verdict(&decode(&fixture_status(false, "edited-revision")), &index),
            StatusVerdict::SourceChanged
        );
        // Not fresh although the checkout still matches what was published:
        // a digest-contract or admission mismatch, not an edit.
        assert_eq!(
            status_verdict(
                &decode(&fixture_status(false, "published-revision")),
                &index
            ),
            StatusVerdict::Unproven
        );
        let mut without_live_revision = fixture_status(false, "edited-revision");
        without_live_revision["project"]["live_source_revision"] = serde_json::Value::Null;
        assert_eq!(
            status_verdict(&decode(&without_live_revision), &index),
            StatusVerdict::Unproven
        );
        // Another writer replaced the generation this upgrade published.
        let mut superseded = fixture_status(false, "edited-revision");
        superseded["project"]["snapshot"]["current"]["generation_id"] =
            serde_json::json!(OTHER_GENERATION);
        assert_eq!(
            status_verdict(&decode(&superseded), &index),
            StatusVerdict::Superseded
        );
    }

    #[test]
    fn managed_replacement_steps_preserve_backup_confirmation_and_resume() {
        let mut reconciliation = blocked_project_reconciliation("fixture blocked", Some(55_433));
        reconciliation.required_confirmation = Some("upgrade-managed-database");
        let mut steps = Vec::new();
        add_project_reconciliation_steps(&mut steps, Some(&reconciliation));
        assert_eq!(steps.len(), 3);
        assert!(steps[0].contains("db backup"));
        assert!(steps[0].contains("--port 55433"));
        assert!(steps[1].contains("--confirm upgrade-managed-database"));
        assert!(steps[2].contains("upgrade --apply"));
        assert!(steps[2].contains("resume"));
    }

    #[test]
    fn managed_start_timeout_retries_without_claiming_incompatibility() {
        let mut reconciliation =
            started_project_reconciliation(ProjectDatabaseMode::Managed(55_433));
        reconciliation.database = upgrade_step(
            "timed_out",
            "fixture cold managed start exceeded its readiness budget",
        );
        let mut steps = Vec::new();
        add_project_reconciliation_steps(&mut steps, Some(&reconciliation));
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("timed out"));
        assert!(steps[0].contains("upgrade --apply"));
        assert!(!steps[0].contains("--confirm"));
        assert!(!steps[0].contains("doctor"));
    }

    /// Write an executable fake `cartograph` that dispatches on its subcommand.
    #[cfg(unix)]
    fn fixture_cartograph(
        directory: &Path,
        script: &str,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let executable = directory.join("fixture-cartograph");
        fs::write(&executable, script)?;
        let mut permissions = fs::metadata(&executable)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions)?;
        Ok(executable)
    }

    #[cfg(unix)]
    async fn reconcile_with_fixture(
        script: &str,
    ) -> Result<ProjectReconciliation, Box<dyn std::error::Error>> {
        let project = tempfile::tempdir()?;
        let executable = fixture_cartograph(project.path(), script)?;
        Ok(reconcile_project_with(CompletionProjectInput {
            executable: &executable,
            project_path: project.path(),
            installed_version: "2.1.7",
            database_mode: ProjectDatabaseMode::External,
            index_policy: DEFAULT_INDEX_CHILD_POLICY,
        })
        .await)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_reconciliation_runs_index_doctor_and_next_process_status_in_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let report = reconcile_with_fixture(
            r#"#!/bin/sh
case "$1" in
  index)
    test "$3" = --preserve-current-excludes && test "$4" = --supervised || exit 24
    : > "$PWD/.upgrade-indexed"
    printf '%s\n' '{"generation_id":"11111111-1111-4111-8111-111111111111","source_revision":"rev-1","live_source":"matched"}'
    ;;
  doctor)
    test -f "$PWD/.upgrade-indexed" || exit 21
    : > "$PWD/.upgrade-doctored"
    ;;
  status)
    test -f "$PWD/.upgrade-doctored" || exit 22
    printf '%s\n' '{"version":"2.1.7","project":{"fresh":true,"live_source_revision":"rev-1","snapshot":{"current":{"generation_id":"11111111-1111-4111-8111-111111111111","source_revision":"rev-1"}}}}'
    ;;
  *)
    exit 23
    ;;
esac
"#,
        )
        .await?;
        assert_eq!(report.state, "ready");
        assert!(!report.retryable);
        assert_eq!(report.database.state, "ready");
        assert_eq!(report.index.state, "ready");
        assert_eq!(report.doctor.state, "ready");
        assert_eq!(report.verification.state, "ready");
        assert!(report.fresh);
        assert!(report.upgrade_completed());
        assert_eq!(
            report.generation_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_checkout_edited_after_publication_completes_the_upgrade_as_source_changed()
    -> Result<(), Box<dyn std::error::Error>> {
        let report = reconcile_with_fixture(
            r#"#!/bin/sh
case "$1" in
  index)
    printf '%s\n' '{"generation_id":"11111111-1111-4111-8111-111111111111","source_revision":"rev-1","live_source":"changed_after_publication"}'
    ;;
  doctor)
    ;;
  status)
    printf '%s\n' '{"version":"2.1.7","project":{"fresh":false,"live_source_revision":"rev-2","snapshot":{"current":{"generation_id":"11111111-1111-4111-8111-111111111111","source_revision":"rev-1"}}}}'
    ;;
  *)
    exit 23
    ;;
esac
"#,
        )
        .await?;
        assert_eq!(report.state, RECONCILED_SOURCE_CHANGED);
        assert_eq!(report.index.state, RECONCILED_SOURCE_CHANGED);
        assert_eq!(report.verification.state, RECONCILED_SOURCE_CHANGED);
        assert!(!report.fresh);
        assert!(!report.retryable);
        assert!(report.upgrade_completed());
        assert_eq!(
            report.generation_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        let mut steps = Vec::new();
        add_project_reconciliation_steps(&mut steps, Some(&report));
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("upgrade is complete"));
        assert!(steps[0].contains("cartograph index"));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn another_live_writer_is_a_distinct_retryable_outcome()
    -> Result<(), Box<dyn std::error::Error>> {
        let report = reconcile_with_fixture(
            r#"#!/bin/sh
case "$1" in
  index)
    printf '{\n  "error": {\n    "code": "lease_busy",\n    "message": "busy",\n    "previous_generation_visible": true\n  }\n}\n' >&2
    exit 1
    ;;
  *)
    exit 23
    ;;
esac
"#,
        )
        .await?;
        assert_eq!(report.state, "blocked");
        assert!(report.retryable);
        assert_eq!(report.index.state, ANOTHER_WRITER_ACTIVE);
        assert_eq!(report.index.reason.as_deref(), Some(LEASE_BUSY_CODE));
        assert_eq!(report.doctor.state, "not_run");
        assert!(!report.upgrade_completed());
        let mut steps = Vec::new();
        add_project_reconciliation_steps(&mut steps, Some(&report));
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("kept this project busy"));
        assert!(steps[0].contains("upgrade --apply"));
        assert!(!steps[0].contains("doctor"));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_index_failure_reports_the_child_code_instead_of_a_generic_block()
    -> Result<(), Box<dyn std::error::Error>> {
        let report = reconcile_with_fixture(
            r#"#!/bin/sh
case "$1" in
  index)
    printf '{\n  "error": {\n    "code": "parse_failed"\n  }\n}\n' >&2
    exit 1
    ;;
  *)
    exit 23
    ;;
esac
"#,
        )
        .await?;
        assert_eq!(report.index.state, "blocked");
        assert_eq!(report.index.reason.as_deref(), Some("parse_failed"));
        assert!(!report.retryable);
        Ok(())
    }

    #[test]
    fn index_timeouts_name_the_bound_and_whether_cleanup_was_confirmed() {
        let step = |stop| {
            index_step(&IndexChildOutcome::TimedOut(IndexChildTimeout {
                trigger: index_child::DeadlineTrigger::NoProgress,
                stop,
            }))
        };
        let (cooperative, retryable) = step(ChildStop::Cooperative);
        assert!(retryable);
        assert_eq!(cooperative.state, "timed_out");
        assert_eq!(cooperative.reason.as_deref(), Some("no_progress"));
        assert!(cooperative.message.contains("confirmed that it released"));
        let (unconfirmed, _) = step(ChildStop::Unconfirmed);
        assert!(!unconfirmed.message.contains("confirmed that it released"));
        assert!(unconfirmed.message.contains("TTL"));
        let (forced, _) = step(ChildStop::Forced);
        assert!(forced.message.contains("killed"));
        let (ceiling, _) = index_step(&IndexChildOutcome::TimedOut(IndexChildTimeout {
            trigger: index_child::DeadlineTrigger::Ceiling,
            stop: ChildStop::Cooperative,
        }));
        assert_eq!(ceiling.reason.as_deref(), Some("ceiling"));
        assert!(ceiling.message.contains("180-minute"));
    }

    #[test]
    fn automatic_registration_repair_preserves_the_existing_argument_payload() {
        let registration = InstallTargetDetection {
            target: "codex",
            location: "local",
            config_present: true,
            config_valid: true,
            cartograph_configured: true,
            config_path: ".codex/config.toml",
            command_state: "stale_absolute",
            wrapped_executable_state: None,
            managed_database_port: Some(55_435),
            repin_command: None,
            executable: None,
        };
        assert_eq!(
            registration_repair_args(&registration),
            vec![
                "install".to_owned(),
                "--yes".to_owned(),
                "--no-permissions".to_owned(),
                "--no-hooks".to_owned(),
                "--target".to_owned(),
                "codex".to_owned(),
                "--location".to_owned(),
                "local".to_owned(),
                "--managed-database-port".to_owned(),
                "55435".to_owned(),
                "--project-path".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    fn write_fixture(path: &Path, contents: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)
    }

    #[cfg(unix)]
    fn read_json(path: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    /// Temporary project and home whose stale registrations a repair run
    /// inspects; the installed executable only records its invocations.
    #[cfg(unix)]
    struct RepairFixture {
        _directories: [tempfile::TempDir; 2],
        project_root: PathBuf,
        home_root: PathBuf,
        executable: PathBuf,
        pinned: String,
        stale: String,
    }

    #[cfg(unix)]
    impl RepairFixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let directories = [tempfile::tempdir()?, tempfile::tempdir()?];
            let project_root = directories[0].path().canonicalize()?;
            let home_root = directories[1].path().canonicalize()?;
            let executable = project_root.join("installed/cartograph");
            write_fixture(
                &executable,
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/invocations.log\"\n",
            )?;
            let mut permissions = fs::metadata(&executable)?.permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&executable, permissions)?;
            let pinned = executable.canonicalize()?.to_string_lossy().into_owned();
            let stale = project_root
                .join(".cartograph-cli/versions/v2.1.30/bin/cartograph")
                .to_string_lossy()
                .into_owned();
            Ok(Self {
                _directories: directories,
                project_root,
                home_root,
                executable,
                pinned,
                stale,
            })
        }

        async fn repair(&self) -> RegistrationRepair {
            repair_registrations(&RegistrationRepairInput {
                executable: &self.executable,
                scope: HostScope {
                    project_root: &self.project_root,
                    home: Some(&self.home_root),
                    location: DiagnosticLocation::Both,
                },
            })
            .await
        }

        fn invocations(&self) -> String {
            fs::read_to_string(self.project_root.join("installed/invocations.log"))
                .unwrap_or_default()
        }
    }

    #[cfg(unix)]
    fn reported_change<'report>(
        report: &'report RegistrationRepair,
        target: &str,
        location: &str,
    ) -> &'report RegistrationChange {
        report
            .changes
            .iter()
            .find(|change| change.target == target && change.location == location)
            .unwrap_or_else(|| panic!("no repair reported for {target} {location}"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upgrade_repair_repins_wrapped_entries_in_place_and_reinstalls_direct_pins()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = RepairFixture::new()?;
        let (stale, pinned) = (fixture.stale.as_str(), fixture.pinned.as_str());
        let root_text = fixture.project_root.to_string_lossy().into_owned();
        let cursor_entry = serde_json::json!({
            "command": "/usr/bin/env",
            "args": [stale, "serve", "--mcp", "--project-path", root_text],
            "env": {"EXAMPLE_FLAG": "fixture-env-value"},
            "cwd": root_text
        });
        let cursor = fixture.project_root.join(".cursor/mcp.json");
        write_fixture(
            &cursor,
            &serde_json::json!({"mcpServers": {"cartograph": cursor_entry}}).to_string(),
        )?;
        write_fixture(
            &fixture.project_root.join(".codex/config.toml"),
            &format!(
                "[mcp_servers.cartograph]\ncommand = \"{stale}\"\nargs = [\"serve\", \"--mcp\", \"--project-path\", \"{root_text}\"]\n"
            ),
        )?;
        let claude_entry = serde_json::json!({
            "command": "op",
            "args": ["run", "--", stale, "serve", "--mcp"],
            "env": {"EXAMPLE_FLAG": "fixture-env-value"}
        });
        let claude = fixture.home_root.join(".claude.json");
        write_fixture(
            &claude,
            &serde_json::json!({"mcpServers": {"cartograph": claude_entry}}).to_string(),
        )?;

        let report = fixture.repair().await;

        assert_eq!(
            (report.attempted, report.repaired, report.failed),
            (3, 3, 0)
        );
        let mut expected_cursor = cursor_entry;
        expected_cursor["args"][0] = serde_json::json!(pinned);
        assert_eq!(
            read_json(&cursor)?.pointer("/mcpServers/cartograph"),
            Some(&expected_cursor)
        );
        let cursor_change = reported_change(&report, "cursor", "local");
        assert_eq!(
            (
                cursor_change.outcome,
                cursor_change.command_state,
                cursor_change.field.as_str(),
                cursor_change.wrapper.as_deref(),
            ),
            ("repinned", "wrapped", "args[0]", Some("/usr/bin/env"))
        );
        assert_eq!(
            (cursor_change.from.as_str(), cursor_change.to.as_str()),
            (stale, pinned)
        );

        let mut expected_claude = claude_entry;
        expected_claude["args"][2] = serde_json::json!(pinned);
        assert_eq!(
            read_json(&claude)?.pointer("/mcpServers/cartograph"),
            Some(&expected_claude)
        );
        assert_eq!(
            reported_change(&report, "claude", "global").field,
            "args[2]"
        );

        let codex_change = reported_change(&report, "codex", "local");
        assert_eq!(
            (codex_change.outcome, codex_change.field.as_str()),
            ("repinned", "command")
        );
        assert_eq!(codex_change.wrapper, None);
        assert_eq!(
            fixture.invocations(),
            format!(
                "install --yes --no-permissions --no-hooks --target codex --location local --project-path {root_text}\n"
            )
        );

        assert!(!serde_json::to_string(&report)?.contains("fixture-env-value"));
        let mut rendered = String::new();
        render_registration_repair(&mut rendered, &report);
        assert!(rendered.contains(&format!(
            "cursor local (.cursor/mcp.json): repinned args[0] {stale} -> {pinned}; wrapper /usr/bin/env"
        )));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upgrade_repair_reports_a_manual_step_when_a_wrapper_cannot_be_repinned_safely()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let fixture = RepairFixture::new()?;
        let elsewhere = tempfile::tempdir()?;
        let (stale, pinned) = (fixture.stale.as_str(), fixture.pinned.as_str());
        let wrapped = serde_json::json!({"mcpServers": {"cartograph": {
            "command": "/usr/bin/env",
            "args": [stale, "serve", "--mcp"]
        }}})
        .to_string();
        write_fixture(&elsewhere.path().join("mcp.json"), &wrapped)?;
        symlink(elsewhere.path(), fixture.home_root.join(".cursor"))?;

        let report = fixture.repair().await;

        assert_eq!(
            (report.attempted, report.repaired, report.failed),
            (1, 0, 1)
        );
        let manual = reported_change(&report, "cursor", "global");
        assert_eq!(manual.outcome, "manual");
        let step = manual.manual_step.as_deref().unwrap_or_default();
        assert!(step.contains("~/.cursor/mcp.json"));
        assert!(step.contains(&format!("args[0] `{stale}` with `{pinned}`")));
        assert_eq!(
            fs::read_to_string(elsewhere.path().join("mcp.json"))?,
            wrapped
        );
        assert_eq!(fixture.invocations(), "");
        let mut rendered = String::new();
        render_registration_repair(&mut rendered, &report);
        assert!(rendered.contains("cursor global (~/.cursor/mcp.json): manual: Edit"));
        Ok(())
    }

    #[test]
    fn staged_binary_launch_retries_only_executable_file_busy() {
        let mut transient_attempts = 0;
        let transient = verify_staged_binary_with("2.0.8", || {
            transient_attempts += 1;
            if transient_attempts < MAXIMUM_STAGED_BINARY_LAUNCH_ATTEMPTS {
                Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
            } else {
                Ok(StagedBinaryOutput {
                    success: true,
                    stdout: b"cartograph 2.0.8\n".to_vec(),
                })
            }
        });
        assert_eq!(transient, Ok(()));
        assert_eq!(transient_attempts, MAXIMUM_STAGED_BINARY_LAUNCH_ATTEMPTS);

        let mut persistent_attempts = 0;
        let persistent = verify_staged_binary_with("2.0.8", || {
            persistent_attempts += 1;
            Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
        });
        assert_eq!(
            persistent,
            Err("downloaded native binary could not start".to_owned())
        );
        assert_eq!(persistent_attempts, MAXIMUM_STAGED_BINARY_LAUNCH_ATTEMPTS);

        let mut permanent_attempts = 0;
        let permanent = verify_staged_binary_with("2.0.8", || {
            permanent_attempts += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        assert_eq!(
            permanent,
            Err("downloaded native binary could not start".to_owned())
        );
        assert_eq!(permanent_attempts, 1);
    }

    #[tokio::test]
    async fn release_download_is_status_content_length_and_stream_bounded() {
        let (url, request) = spawn_http_fixture(
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata".to_owned(),
        );
        let client = http_client().unwrap_or_else(|error| panic!("HTTP client failed: {error}"));
        assert_eq!(
            fetch_bounded(&client, &url, 4)
                .await
                .unwrap_or_else(|error| panic!("bounded fetch failed: {error}")),
            b"data"
        );
        request
            .join()
            .unwrap_or_else(|_| panic!("download fixture panicked"));

        for response in [
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
        ] {
            let (url, request) = spawn_http_fixture(response.to_owned());
            assert!(fetch_bounded(&client, &url, 4).await.is_err());
            request
                .join()
                .unwrap_or_else(|_| panic!("rejected download fixture panicked"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn staged_native_update_is_executable_verified_and_atomically_replaced() {
        use std::os::unix::fs::PermissionsExt as _;

        let root =
            tempfile::tempdir().unwrap_or_else(|error| panic!("upgrade fixture failed: {error}"));
        let executable = root.path().join("cartograph");
        fs::write(&executable, "#!/bin/sh\necho 'cartograph 1.0.0'\n")
            .unwrap_or_else(|error| panic!("old executable fixture failed: {error}"));
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("old executable chmod failed: {error}"));
        let replacement = b"#!/bin/sh\necho 'cartograph 9.9.9'\n";
        install_binary(&executable, replacement, "9.9.9")
            .unwrap_or_else(|error| panic!("atomic install failed: {error}"));
        assert_eq!(
            fs::read(&executable)
                .unwrap_or_else(|error| panic!("installed executable read failed: {error}")),
            replacement
        );
        assert!(verify_staged_binary(&executable, "9.9.9").is_ok());
        assert!(verify_staged_binary(&executable, "1.0.0").is_err());
        let mode = fs::metadata(&executable)
            .unwrap_or_else(|error| panic!("installed executable metadata failed: {error}"))
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }

    #[cfg(unix)]
    #[test]
    fn versioned_native_update_installs_a_new_release_and_preserves_the_prior_binary() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root =
            tempfile::tempdir().unwrap_or_else(|error| panic!("upgrade fixture failed: {error}"));
        let install_root = root.path().join(".cartograph-cli");
        let old_release = install_root.join("versions/v2.0.7");
        let old_executable = old_release.join("bin/cartograph");
        fs::create_dir_all(
            old_executable
                .parent()
                .unwrap_or_else(|| panic!("old executable fixture has no parent")),
        )
        .unwrap_or_else(|error| panic!("old release fixture failed: {error}"));
        let old_bytes = b"#!/bin/sh\necho 'cartograph 2.0.7'\n";
        fs::write(&old_executable, old_bytes)
            .unwrap_or_else(|error| panic!("old executable fixture failed: {error}"));
        fs::set_permissions(&old_executable, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("old executable chmod failed: {error}"));
        symlink("versions/v2.0.7", install_root.join("current"))
            .unwrap_or_else(|error| panic!("current release fixture failed: {error}"));
        let launcher_directory = root.path().join("bin");
        fs::create_dir(&launcher_directory)
            .unwrap_or_else(|error| panic!("launcher directory fixture failed: {error}"));
        let launcher = launcher_directory.join("cartograph");
        symlink(&old_executable, &launcher)
            .unwrap_or_else(|error| panic!("launcher fixture failed: {error}"));

        let launcher_path = env::join_paths([&launcher_directory])
            .unwrap_or_else(|error| panic!("launcher PATH fixture failed: {error}"));
        let replacement = b"#!/bin/sh\necho 'cartograph 2.0.8'\n";
        install_binary_unix(&InstallBinaryInput {
            executable: &old_executable,
            bytes: replacement,
            version: "2.0.8",
            launcher_path: Some(&launcher_path),
        })
        .unwrap_or_else(|error| panic!("versioned install failed: {error}"));

        let new_release = install_root.join("versions/v2.0.8");
        let new_executable = new_release.join("bin/cartograph");
        assert_eq!(
            fs::read(&old_executable)
                .unwrap_or_else(|error| panic!("prior executable read failed: {error}")),
            old_bytes
        );
        assert_eq!(
            fs::read(&new_executable)
                .unwrap_or_else(|error| panic!("new executable read failed: {error}")),
            replacement
        );
        assert_eq!(
            fs::canonicalize(install_root.join("current"))
                .unwrap_or_else(|error| panic!("current release resolve failed: {error}")),
            fs::canonicalize(new_release)
                .unwrap_or_else(|error| panic!("new release resolve failed: {error}"))
        );
        assert_eq!(
            fs::canonicalize(launcher)
                .unwrap_or_else(|error| panic!("launcher resolve failed: {error}")),
            fs::canonicalize(new_executable)
                .unwrap_or_else(|error| panic!("new executable resolve failed: {error}"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn versioned_native_update_remains_applied_when_legacy_launcher_repoint_fails() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root =
            tempfile::tempdir().unwrap_or_else(|error| panic!("upgrade fixture failed: {error}"));
        let install_root = root.path().join(".cartograph-cli");
        let old_release = install_root.join("versions/v2.0.7");
        let old_executable = old_release.join("bin/cartograph");
        fs::create_dir_all(
            old_executable
                .parent()
                .unwrap_or_else(|| panic!("old executable fixture has no parent")),
        )
        .unwrap_or_else(|error| panic!("old release fixture failed: {error}"));
        fs::write(&old_executable, "#!/bin/sh\necho 'cartograph 2.0.7'\n")
            .unwrap_or_else(|error| panic!("old executable fixture failed: {error}"));
        fs::set_permissions(&old_executable, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("old executable chmod failed: {error}"));
        symlink("versions/v2.0.7", install_root.join("current"))
            .unwrap_or_else(|error| panic!("current release fixture failed: {error}"));

        let launcher_directory = root.path().join("bin");
        fs::create_dir(&launcher_directory)
            .unwrap_or_else(|error| panic!("launcher directory fixture failed: {error}"));
        let launcher = launcher_directory.join("cartograph");
        symlink(&old_executable, &launcher)
            .unwrap_or_else(|error| panic!("launcher fixture failed: {error}"));
        fs::set_permissions(&launcher_directory, fs::Permissions::from_mode(0o555))
            .unwrap_or_else(|error| panic!("launcher directory chmod failed: {error}"));

        let launcher_path = env::join_paths([&launcher_directory])
            .unwrap_or_else(|error| panic!("launcher PATH fixture failed: {error}"));
        let replacement = b"#!/bin/sh\necho 'cartograph 2.0.8'\n";
        let result = install_binary_unix(&InstallBinaryInput {
            executable: &old_executable,
            bytes: replacement,
            version: "2.0.8",
            launcher_path: Some(&launcher_path),
        });
        fs::set_permissions(&launcher_directory, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("launcher directory restore failed: {error}"));

        let installed =
            result.unwrap_or_else(|error| panic!("completed install was reported failed: {error}"));
        assert_eq!(
            installed.launcher_warning.as_deref(),
            Some("could not stage the Cartograph launcher link")
        );
        let new_release = install_root.join("versions/v2.0.8");
        assert_eq!(installed.path, new_release.join("bin/cartograph"));
        assert_eq!(
            fs::canonicalize(install_root.join("current"))
                .unwrap_or_else(|error| panic!("current release resolve failed: {error}")),
            fs::canonicalize(new_release)
                .unwrap_or_else(|error| panic!("new release resolve failed: {error}"))
        );
        assert_eq!(
            fs::canonicalize(launcher)
                .unwrap_or_else(|error| panic!("launcher resolve failed: {error}")),
            fs::canonicalize(old_executable)
                .unwrap_or_else(|error| panic!("old executable resolve failed: {error}"))
        );
    }

    fn spawn_http_fixture(response: String) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("upgrade fixture bind failed: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("upgrade fixture address failed: {error}"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("upgrade fixture accept failed: {error}"));
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("upgrade fixture timeout failed: {error}"));
            let mut request = vec![0_u8; FIXTURE_REQUEST_BYTES];
            let read = stream
                .read(&mut request)
                .unwrap_or_else(|error| panic!("upgrade fixture read failed: {error}"));
            request.truncate(read);
            stream
                .write_all(response.as_bytes())
                .and_then(|()| stream.flush())
                .unwrap_or_else(|error| panic!("upgrade fixture response failed: {error}"));
            request
        });
        (format!("http://{address}/release"), handle)
    }
}
