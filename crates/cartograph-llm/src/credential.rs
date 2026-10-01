//! Shell-free credential commands for remote LLM tiers.
//!
//! A tier may name an argv (`apiKeyCommand`) whose standard output is its
//! credential. Cartograph runs it directly, without a shell, the first time a
//! request needs the credential inside the serving process and keeps the value
//! only in that process's memory. A provider rejection re-runs the command once,
//! so a rotated key is picked up without a restart. The value and the command's
//! standard error are never logged, persisted, serialized or reported.

use std::{
    ffi::OsStr,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use reqwest::{StatusCode, header::HeaderValue};
use secrecy::{ExposeSecret as _, SecretString, zeroize::Zeroizing};
use thiserror::Error;
use tokio::{io::AsyncReadExt as _, process::Command};

use crate::ProjectLlmConfigError;

/// Deadline for one credential command run, including reading its output.
const CREDENTIAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest standard output accepted from a credential command.
const MAXIMUM_CREDENTIAL_OUTPUT_BYTES: usize = 4 * 1024;
/// Minimum interval before re-running a command whose last run failed, or
/// whose fresh output the provider rejected again.
const RERUN_INTERVAL: Duration = Duration::from_secs(30);
/// Distinct credential commands one process keeps resolved.
const MAXIMUM_CACHED_COMMANDS: usize = 16;

/// Validated shell-free argv whose standard output is one tier credential.
///
/// Configuration stores only the argv. It is executed directly, never through
/// a shell, so shell syntax in an argument is passed through literally.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialCommand {
    program: String,
    args: Vec<String>,
}

impl std::fmt::Debug for CredentialCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialCommand")
            .field("program", &self.program_name())
            .field("argument_count", &self.args.len())
            .finish()
    }
}

impl CredentialCommand {
    /// Validate one argv: a program followed by its arguments, under the same
    /// text, count and byte bounds as the CLI bridge command and arguments.
    /// # Errors
    ///
    /// Returns [`ProjectLlmConfigError::InvalidTier`] for an empty argv or
    /// empty, oversized, control-character or too many arguments.
    pub fn new(argv: Vec<String>) -> Result<Self, ProjectLlmConfigError> {
        let mut parts = argv.into_iter();
        let program = parts.next().ok_or(ProjectLlmConfigError::InvalidTier)?;
        let arguments = parts.collect::<Vec<_>>();
        crate::project_config::validate_process_argv(&program, &arguments)?;
        Ok(Self {
            program,
            args: arguments,
        })
    }

    /// Executable passed directly to the operating system.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Arguments passed without shell interpretation.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Final path component of the program. Diagnostics name only this, never
    /// the arguments, the directory or the output.
    #[must_use]
    pub fn program_name(&self) -> &str {
        Path::new(&self.program)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(&self.program)
    }

    /// Run the command once, outside the process cache, and report only
    /// whether it produced a usable credential. `doctor` and `llm smoke` use
    /// this to check the credential the MCP server would resolve.
    /// # Errors
    ///
    /// Returns a redacted failure naming the program and the failure category.
    pub async fn verify(&self) -> Result<(), CredentialCommandError> {
        run_credential_command(self, CommandLimits::DEFAULT)
            .await
            .map(drop)
            .map_err(|failure| self.error(failure))
    }

    /// Resolve through the process cache. Concurrent first uses share one run,
    /// and a failure is remembered for `RERUN_INTERVAL` so a hanging helper
    /// does not stall every request.
    pub(crate) async fn resolve(&self) -> Result<SecretString, CredentialCommandError> {
        self.resolve_with(CommandLimits::DEFAULT).await
    }

    async fn resolve_with(
        &self,
        limits: CommandLimits,
    ) -> Result<SecretString, CredentialCommandError> {
        let slot = cache_slot(self);
        let mut cached = slot.lock().await;
        match &*cached {
            CachedCredential::Resolved { secret, .. } => return Ok(secret.clone()),
            CachedCredential::Failed { failure, at } if at.elapsed() < RERUN_INTERVAL => {
                return Err(self.error(*failure));
            }
            CachedCredential::Unresolved | CachedCredential::Failed { .. } => {}
        }
        let secret = self.run_into(&mut cached, limits).await?;
        *cached = CachedCredential::Resolved {
            secret: secret.clone(),
            rerun_at: None,
        };
        Ok(secret)
    }

    /// After the provider rejected `rejected`, run the command again unless
    /// another request already replaced that value, and return a credential
    /// only when it differs from the rejected one.
    pub(crate) async fn refresh_rejected(
        &self,
        rejected: &SecretString,
    ) -> Result<Option<SecretString>, CredentialCommandError> {
        self.refresh_rejected_with(rejected, CommandLimits::DEFAULT)
            .await
    }

    async fn refresh_rejected_with(
        &self,
        rejected: &SecretString,
        limits: CommandLimits,
    ) -> Result<Option<SecretString>, CredentialCommandError> {
        let slot = cache_slot(self);
        let mut cached = slot.lock().await;
        match &*cached {
            CachedCredential::Resolved { secret, .. } if !same_secret(secret, rejected) => {
                return Ok(Some(secret.clone()));
            }
            CachedCredential::Resolved {
                rerun_at: Some(at), ..
            } if at.elapsed() < RERUN_INTERVAL => return Ok(None),
            CachedCredential::Failed { failure, at } if at.elapsed() < RERUN_INTERVAL => {
                return Err(self.error(*failure));
            }
            CachedCredential::Unresolved
            | CachedCredential::Resolved { .. }
            | CachedCredential::Failed { .. } => {}
        }
        let fresh = self.run_into(&mut cached, limits).await?;
        let changed = !same_secret(&fresh, rejected);
        *cached = CachedCredential::Resolved {
            secret: fresh.clone(),
            rerun_at: (!changed).then(Instant::now),
        };
        Ok(changed.then_some(fresh))
    }

    /// A failure recent enough that the next use would report it without
    /// running the command. Never runs the command or waits for a running one.
    pub(crate) fn recent_failure(&self) -> Option<CredentialCommandError> {
        let slot = cache_slot(self);
        let cached = slot.try_lock().ok()?;
        match &*cached {
            CachedCredential::Failed { failure, at } if at.elapsed() < RERUN_INTERVAL => {
                Some(self.error(*failure))
            }
            CachedCredential::Unresolved
            | CachedCredential::Resolved { .. }
            | CachedCredential::Failed { .. } => None,
        }
    }

    /// Run the command, recording a failure in `cached`; success is recorded
    /// by the caller, which knows whether it replaced a rejected value.
    async fn run_into(
        &self,
        cached: &mut CachedCredential,
        limits: CommandLimits,
    ) -> Result<SecretString, CredentialCommandError> {
        run_credential_command(self, limits)
            .await
            .map_err(|failure| {
                *cached = CachedCredential::Failed {
                    failure,
                    at: Instant::now(),
                };
                self.error(failure)
            })
    }

    fn error(&self, failure: CredentialCommandFailure) -> CredentialCommandError {
        CredentialCommandError {
            program: self.program_name().to_owned(),
            failure,
        }
    }
}

/// Secret-free reason a credential command produced no usable credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialCommandFailure {
    /// The program could not be started.
    SpawnFailed,
    /// The program exited unsuccessfully; `code` is `None` when a signal ended it.
    Exited {
        /// Process exit status, when the platform reports one.
        code: Option<i32>,
    },
    /// The program did not finish within its deadline.
    TimedOut,
    /// Standard output exceeded the credential byte cap.
    OutputTooLarge,
    /// Standard output was empty once trailing whitespace was removed.
    EmptyOutput,
    /// Standard output was not UTF-8 or contained control characters.
    InvalidOutput,
}

impl std::fmt::Display for CredentialCommandFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SpawnFailed => formatter.write_str("could not be started"),
            Self::Exited { code: Some(code) } => write!(formatter, "exited with status {code}"),
            Self::Exited { code: None } => formatter.write_str("was terminated by a signal"),
            Self::TimedOut => formatter.write_str("did not finish within its deadline"),
            Self::OutputTooLarge => formatter.write_str("printed more than a credential may hold"),
            Self::EmptyOutput => formatter.write_str("printed no credential"),
            Self::InvalidOutput => formatter.write_str("printed text that is not a credential"),
        }
    }
}

/// A credential command failure. It names only the program's file name and
/// the failure category, never the arguments, output or standard error.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("credential command `{program}` {failure}")]
pub struct CredentialCommandError {
    program: String,
    failure: CredentialCommandFailure,
}

impl CredentialCommandError {
    /// File name of the program that failed.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Why the program produced no usable credential.
    #[must_use]
    pub const fn failure(&self) -> CredentialCommandFailure {
        self.failure
    }
}

/// Bounds for one credential command run.
#[derive(Clone, Copy)]
pub(crate) struct CommandLimits {
    pub(crate) timeout: Duration,
    pub(crate) maximum_output_bytes: usize,
}

impl CommandLimits {
    pub(crate) const DEFAULT: Self = Self {
        timeout: CREDENTIAL_COMMAND_TIMEOUT,
        maximum_output_bytes: MAXIMUM_CREDENTIAL_OUTPUT_BYTES,
    };
}

/// Run `command` with null stdin and discarded stderr, reading at most one
/// byte past the output cap. Dropping the child kills it, so a timeout or an
/// oversized output never leaves the helper running.
pub(crate) async fn run_credential_command(
    command: &CredentialCommand,
    limits: CommandLimits,
) -> Result<SecretString, CredentialCommandFailure> {
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| CredentialCommandFailure::SpawnFailed)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(CredentialCommandFailure::SpawnFailed)?;
    let read_limit = u64::try_from(limits.maximum_output_bytes)
        .map_or(u64::MAX, |limit| limit.saturating_add(1));
    let completed = tokio::time::timeout(limits.timeout, async {
        let mut output = Zeroizing::new(Vec::new());
        stdout
            .take(read_limit)
            .read_to_end(&mut output)
            .await
            .map_err(|_| CredentialCommandFailure::InvalidOutput)?;
        if output.len() > limits.maximum_output_bytes {
            return Err(CredentialCommandFailure::OutputTooLarge);
        }
        let status = child
            .wait()
            .await
            .map_err(|_| CredentialCommandFailure::SpawnFailed)?;
        Ok((output, status))
    })
    .await
    .map_err(|_| CredentialCommandFailure::TimedOut)?;
    let (output, status) = completed?;
    if !status.success() {
        return Err(CredentialCommandFailure::Exited {
            code: status.code(),
        });
    }
    credential_from_output(&output)
}

/// Trim trailing whitespace and accept only non-empty, control-free UTF-8.
fn credential_from_output(output: &[u8]) -> Result<SecretString, CredentialCommandFailure> {
    let text = std::str::from_utf8(output).map_err(|_| CredentialCommandFailure::InvalidOutput)?;
    let credential = text.trim_end();
    if credential.is_empty() {
        Err(CredentialCommandFailure::EmptyOutput)
    } else if credential.chars().any(char::is_control) {
        Err(CredentialCommandFailure::InvalidOutput)
    } else {
        Ok(SecretString::from(credential))
    }
}

fn same_secret(left: &SecretString, right: &SecretString) -> bool {
    left.expose_secret() == right.expose_secret()
}

/// One command's cached outcome, held only in this process's memory.
enum CachedCredential {
    Unresolved,
    Resolved {
        secret: SecretString,
        /// When a rejection-triggered re-run last returned this same value.
        rerun_at: Option<Instant>,
    },
    Failed {
        failure: CredentialCommandFailure,
        at: Instant,
    },
}

type CacheSlot = Arc<tokio::sync::Mutex<CachedCredential>>;

/// Bounded per-process map from a command to its cached outcome.
#[derive(Default)]
struct CredentialCache {
    slots: Vec<(CredentialCommand, CacheSlot)>,
}

impl CredentialCache {
    fn slot(&mut self, command: &CredentialCommand) -> CacheSlot {
        if let Some((_, slot)) = self.slots.iter().find(|(known, _)| known == command) {
            return slot.clone();
        }
        if self.slots.len() >= MAXIMUM_CACHED_COMMANDS {
            let unused = self
                .slots
                .iter()
                .position(|(_, slot)| Arc::strong_count(slot) == 1)
                .unwrap_or(0);
            self.slots.remove(unused);
        }
        let slot = Arc::new(tokio::sync::Mutex::new(CachedCredential::Unresolved));
        self.slots.push((command.clone(), slot.clone()));
        slot
    }
}

fn cache_slot(command: &CredentialCommand) -> CacheSlot {
    static CACHE: OnceLock<Mutex<CredentialCache>> = OnceLock::new();
    CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .slot(command)
}

/// Credential material for one remote tier, resolved when a request needs it.
#[derive(Clone)]
pub(crate) enum TierCredential {
    /// The tier sends no credential.
    None,
    /// A value read from the environment or a legacy inline key.
    Static(SecretString),
    /// A command run lazily inside this process.
    Command(CredentialCommand),
}

impl std::fmt::Debug for TierCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Static(_) => formatter.write_str("Static(<redacted>)"),
            Self::Command(command) => formatter.debug_tuple("Command").field(command).finish(),
        }
    }
}

impl TierCredential {
    /// Wrap an optional already-validated key.
    pub(crate) fn from_key(key: Option<SecretString>) -> Self {
        key.map_or(Self::None, Self::Static)
    }

    /// Whether requests carry a credential at all.
    pub(crate) const fn is_configured(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// The value to send now, running a command source through the cache. The
    /// process future is boxed so provider request futures stay small.
    pub(crate) async fn current(&self) -> Result<Option<SecretString>, CredentialCommandError> {
        match self {
            Self::None => Ok(None),
            Self::Static(secret) => Ok(Some(secret.clone())),
            Self::Command(command) => Box::pin(command.resolve()).await.map(Some),
        }
    }

    /// A replacement for a rejected value; only a command source can supply one.
    async fn replacement(
        &self,
        rejected: Option<&SecretString>,
    ) -> Result<Option<SecretString>, CredentialCommandError> {
        match (self, rejected) {
            (Self::Command(command), Some(rejected)) => {
                Box::pin(command.refresh_rejected(rejected)).await
            }
            _ => Ok(None),
        }
    }

    /// Feed a stable identity into a transport-registry key. A command source
    /// contributes its argv, never a resolved value.
    pub(crate) fn hash_identity(&self, digest: &mut blake3::Hasher) {
        let (tag, parts): (u8, Vec<&str>) = match self {
            Self::None => (0, Vec::new()),
            Self::Static(secret) => (1, vec![secret.expose_secret()]),
            Self::Command(command) => (
                2,
                std::iter::once(command.program.as_str())
                    .chain(command.args.iter().map(String::as_str))
                    .collect(),
            ),
        };
        digest.update(&[tag]);
        for part in parts {
            digest.update(&part.len().to_le_bytes());
            digest.update(part.as_bytes());
        }
    }
}

/// Error categories a credentialed provider request reports.
pub(crate) trait CredentialedRequestError: Sized {
    /// The tier's credential command produced no usable credential.
    fn credential_unavailable(error: CredentialCommandError) -> Self;
    /// The request could not be sent or did not complete.
    fn endpoint_unavailable() -> Self;
}

/// One provider request: the tier credential, the value resolved for it, and
/// how to build the request for a given value.
pub(crate) struct CredentialedRequest<'credential, Build> {
    pub(crate) credential: &'credential TierCredential,
    pub(crate) current: Option<SecretString>,
    pub(crate) build: Build,
}

/// Send one request. When the provider rejects a command-sourced credential,
/// re-run the command once and resend only if it produced a different value.
pub(crate) async fn send_credentialed<Build, E>(
    request: CredentialedRequest<'_, Build>,
) -> Result<reqwest::Response, E>
where
    Build: Fn(Option<&SecretString>) -> Result<reqwest::RequestBuilder, E>,
    E: CredentialedRequestError,
{
    let CredentialedRequest {
        credential,
        current,
        build,
    } = request;
    let response = build(current.as_ref())?
        .send()
        .await
        .map_err(|_| E::endpoint_unavailable())?;
    if !matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Ok(response);
    }
    match credential
        .replacement(current.as_ref())
        .await
        .map_err(E::credential_unavailable)?
    {
        Some(fresh) => build(Some(&fresh))?
            .send()
            .await
            .map_err(|_| E::endpoint_unavailable()),
        None => Ok(response),
    }
}

/// A sensitive `Bearer` authorization value, or `None` when the credential
/// cannot be carried in an HTTP header.
pub(crate) fn bearer_header(credential: &SecretString) -> Option<HeaderValue> {
    let value = Zeroizing::new(format!("Bearer {}", credential.expose_secret()));
    sensitive_header(&value)
}

/// A sensitive header value holding the bare credential.
pub(crate) fn credential_header(credential: &SecretString) -> Option<HeaderValue> {
    sensitive_header(credential.expose_secret())
}

fn sensitive_header(value: &str) -> Option<HeaderValue> {
    let mut header = HeaderValue::from_str(value).ok()?;
    header.set_sensitive(true);
    Some(header)
}

#[cfg(test)]
mod tests;
