use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use cartograph_agent::PipelineStage;
use cartograph_agent::{
    IndexOptions, PipelineFailureReason, ProjectError, ProjectRuntime, ProjectWatchFilter,
};
use cartograph_domain::ContentDigest;
use notify::{Config, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};

use crate::error_codes::{
    GENERATION_CAPACITY_LIMIT, GENERATION_CAPACITY_NEXT_ACTION, GENERATION_CAPACITY_SCOPE,
    is_generation_capacity_failure, project_index_failure_code,
};

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(750);
const MAXIMUM_EVENT_COALESCING_LATENCY: Duration = Duration::from_secs(2);
const MINIMUM_DEBOUNCE_MILLIS: u64 = 50;
const MAXIMUM_DEBOUNCE_MILLIS: u64 = 60_000;
const RECONCILIATION_INTERVAL: Duration = Duration::from_secs(30);
const STARTUP_RECONCILIATION_DELAY: Duration = Duration::from_secs(2);
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(2);
const WATCH_CHANNEL_CAPACITY: usize = 1;
const CONFIG_RELATIVE_PATH: &str = ".cartograph/config.json";
const SCIP_OVERLAY_RELATIVE_PATH: &str = ".cartograph/scip/overlay.scip";
const WATCH_DEBOUNCE_ENV: &str = "CARTOGRAPH_WATCH_DEBOUNCE_MS";
const INITIAL_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const TRANSIENT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const MAXIMUM_RETRY_INTERVAL: Duration = Duration::from_mins(15);
const MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION: u8 = 5;
const MAXIMUM_REPEATED_AUTOMATIC_FAILURES: u8 = 5;

/// Live filesystem watcher plus periodic missed-event reconciliation for one project runtime.
pub(crate) struct ProjectAutoSync {
    watcher: AutoSyncWatcher,
    cancellation: watch::Sender<bool>,
    task: JoinHandle<()>,
    state: Arc<AutoSyncState>,
}

impl ProjectAutoSync {
    pub(crate) fn start(
        runtime: Arc<ProjectRuntime>,
        startup_reconciliation_enabled: bool,
    ) -> Result<Self, AutoSyncError> {
        let root = runtime.project_root_for_host_operations().to_path_buf();
        let (events, receiver) = mpsc::channel(WATCH_CHANNEL_CAPACITY);
        let (cancellation, cancellation_receiver) = watch::channel(false);
        let state = Arc::new(AutoSyncState::default());
        let filter = Arc::new(RwLock::new(
            ProjectWatchFilter::load(&root).map_err(|_| AutoSyncError::InvalidProjectPolicy)?,
        ));
        state.active.store(true, Ordering::Release);
        let watcher = start_watcher(&WatcherContext {
            root,
            events,
            state: state.clone(),
            filter,
        })?;
        let task_state = state.clone();
        let debounce = debounce_from_env();
        let task = tokio::spawn(run_auto_sync(AutoSyncTask {
            runtime,
            events: receiver,
            cancellation: cancellation_receiver,
            event_debounce: EventDebounce {
                quiet_window: debounce,
                maximum_latency: maximum_debounce_latency(debounce),
            },
            state: task_state,
            startup_reconciliation_enabled,
        }));
        Ok(Self {
            watcher,
            cancellation,
            task,
            state,
        })
    }

    pub(crate) fn status(&self) -> AutoSyncStatus {
        let failure = self.state.failure_status();
        let capacity_failure_active = failure.capacity_failure_attempts > 0;
        AutoSyncStatus {
            active: self.state.active.load(Ordering::Acquire),
            backend: self.watcher.backend(),
            events: self.state.events.load(Ordering::Acquire),
            reconciliations: self.state.reconciliations.load(Ordering::Acquire),
            sync_attempts: self.state.sync_attempts.load(Ordering::Acquire),
            publications: self.state.publications.load(Ordering::Acquire),
            noops: self.state.noops.load(Ordering::Acquire),
            errors: self.state.errors.load(Ordering::Acquire),
            last_success_unix_millis: nonzero(
                self.state.last_success_unix_millis.load(Ordering::Acquire),
            ),
            last_error_code: failure.last_error_code,
            last_failure_at: failure.last_failure_at,
            next_retry_at: failure.next_retry_at,
            failed_revision_attempts: failure.attempts,
            retry_suppressed: failure.retry_suppressed.into(),
            repeated_failure_attempts: failure.repeated_failure_attempts,
            repeated_failure_retry_suppressed: failure.repeated_failure_retry_suppressed.into(),
            capacity_failure_attempts: failure.capacity_failure_attempts,
            capacity_retry_suppressed: failure.capacity_retry_suppressed.into(),
            capacity_limit: capacity_failure_active.then_some(GENERATION_CAPACITY_LIMIT),
            capacity_scope: capacity_failure_active.then_some(GENERATION_CAPACITY_SCOPE),
            capacity_next_action: capacity_failure_active
                .then_some(GENERATION_CAPACITY_NEXT_ACTION),
        }
    }
}

impl Drop for ProjectAutoSync {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);
        let _ = self.cancellation.send(true);
        self.task.abort();
    }
}

#[derive(Default)]
struct AutoSyncState {
    active: AtomicBool,
    events: AtomicU64,
    reconciliations: AtomicU64,
    sync_attempts: AtomicU64,
    publications: AtomicU64,
    noops: AtomicU64,
    errors: AtomicU64,
    last_success_unix_millis: AtomicU64,
    failure: RwLock<AutoSyncFailureState>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AutoSyncFailureStatus {
    last_error_code: Option<&'static str>,
    last_failure_at: Option<u64>,
    next_retry_at: Option<u64>,
    attempts: u8,
    retry_suppressed: bool,
    repeated_failure_error_code: Option<&'static str>,
    repeated_failure_attempts: u8,
    repeated_failure_retry_suppressed: bool,
    capacity_failure_attempts: u8,
    capacity_retry_suppressed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AutoSyncFailureState {
    failed_revision: Option<FailedRevision>,
    persistent_revision_attempts: u8,
    status: AutoSyncFailureStatus,
}

impl AutoSyncFailureState {
    fn next_revision_attempts(&self, revision: &FailedRevision, error: &ProjectError) -> (u8, u8) {
        let (attempts, persistent_attempts) = if self.failed_revision.as_ref() == Some(revision) {
            (self.status.attempts, self.persistent_revision_attempts)
        } else {
            (0, 0)
        };
        let persistent_attempts = if recoverable_index_interruption(error) {
            persistent_attempts
        } else {
            persistent_attempts.saturating_add(1)
        };
        (attempts.saturating_add(1), persistent_attempts)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FailedRevision {
    Known(ContentDigest),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventSyncRoute {
    DirectIndex,
    CheckFreshness,
    WaitForRetry,
}

impl AutoSyncState {
    fn failure_status(&self) -> AutoSyncFailureStatus {
        self.failure.read().map_or_else(
            |_| AutoSyncFailureStatus::default(),
            |failure| failure.status,
        )
    }

    fn automatic_attempt_allowed(&self, revision: &ContentDigest, now: u64) -> bool {
        self.failure.read().map_or(true, |failure| {
            if failure.status.capacity_retry_suppressed
                || failure.status.repeated_failure_retry_suppressed
            {
                return false;
            }
            match failure.failed_revision.as_ref() {
                Some(FailedRevision::Known(failed_revision)) if failed_revision != revision => true,
                Some(_) => retry_allowed(failure.status, now),
                None => true,
            }
        })
    }

    fn event_sync_route(&self, now: u64) -> EventSyncRoute {
        self.failure
            .read()
            .map_or(EventSyncRoute::CheckFreshness, |failure| {
                match failure.failed_revision.as_ref() {
                    None => EventSyncRoute::DirectIndex,
                    Some(FailedRevision::Known(_)) => EventSyncRoute::CheckFreshness,
                    Some(FailedRevision::Unknown) if retry_allowed(failure.status, now) => {
                        EventSyncRoute::CheckFreshness
                    }
                    Some(FailedRevision::Unknown) => EventSyncRoute::WaitForRetry,
                }
            })
    }

    fn reconciliation_probe_allowed(&self, now: u64) -> bool {
        self.failure.read().map_or(true, |failure| {
            !matches!(
                failure.failed_revision.as_ref(),
                Some(FailedRevision::Unknown)
            ) || retry_allowed(failure.status, now)
        })
    }

    fn record_index_success(&self) {
        if let Ok(mut failure) = self.failure.write() {
            *failure = AutoSyncFailureState::default();
        }
    }

    fn record_index_failure(&self, revision: ContentDigest, error: &ProjectError, now: u64) {
        self.record_failure(FailedRevision::Known(revision), error, now);
    }

    fn record_unknown_failure(&self, error: &ProjectError, now: u64) {
        self.record_failure(FailedRevision::Unknown, error, now);
    }

    fn record_failure(&self, revision: FailedRevision, error: &ProjectError, now: u64) {
        self.errors.fetch_add(1, Ordering::AcqRel);
        let Ok(mut failure) = self.failure.write() else {
            return;
        };
        let (attempts, persistent_revision_attempts) =
            failure.next_revision_attempts(&revision, error);
        let error_code = project_error_code(error);
        let transient = recoverable_index_interruption(error);
        let (
            repeated_failure_error_code,
            repeated_failure_attempts,
            repeated_failure_retry_suppressed,
        ) = if transient || matches!(error, ProjectError::StatusFailed) {
            (
                failure.status.repeated_failure_error_code,
                failure.status.repeated_failure_attempts,
                failure.status.repeated_failure_retry_suppressed,
            )
        } else {
            let repeated_failure_attempts =
                if failure.status.repeated_failure_error_code == Some(error_code) {
                    failure.status.repeated_failure_attempts.saturating_add(1)
                } else {
                    1
                };
            (
                Some(error_code),
                repeated_failure_attempts,
                repeated_failure_attempts >= MAXIMUM_REPEATED_AUTOMATIC_FAILURES,
            )
        };
        let capacity_failure_attempts = if is_generation_capacity_failure(error) {
            failure.status.capacity_failure_attempts.saturating_add(1)
        } else {
            failure.status.capacity_failure_attempts
        };
        let capacity_retry_suppressed =
            capacity_failure_attempts >= MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION;
        let retry_suppressed = capacity_retry_suppressed
            || repeated_failure_retry_suppressed
            || (matches!(&revision, FailedRevision::Known(_))
                && persistent_revision_attempts >= MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION);
        let interval = if transient {
            TRANSIENT_RETRY_INTERVAL
                .saturating_mul(2_u32.saturating_pow(u32::from(attempts.saturating_sub(1)).min(4)))
                .min(INITIAL_RETRY_INTERVAL)
        } else {
            retry_interval(persistent_revision_attempts)
        };
        let next_retry_at = (!retry_suppressed)
            .then(|| now.saturating_add(interval.as_millis().try_into().unwrap_or(u64::MAX)));
        *failure = AutoSyncFailureState {
            failed_revision: Some(revision),
            persistent_revision_attempts,
            status: AutoSyncFailureStatus {
                last_error_code: Some(error_code),
                last_failure_at: Some(now),
                next_retry_at,
                attempts,
                retry_suppressed,
                repeated_failure_error_code,
                repeated_failure_attempts,
                repeated_failure_retry_suppressed,
                capacity_failure_attempts,
                capacity_retry_suppressed,
            },
        };
    }
}

fn recoverable_index_interruption(error: &ProjectError) -> bool {
    match error {
        ProjectError::IndexLeaseBusy | ProjectError::SourceChangedDuringIndex => true,
        ProjectError::IndexStageFailedWithReason { reason, .. } => {
            *reason == PipelineFailureReason::SourceChangedDuringParse
        }
        ProjectError::IndexStageFileFailed { failure, .. } => {
            failure.reason() == PipelineFailureReason::SourceChangedDuringParse
        }
        _ => false,
    }
}

fn retry_allowed(status: AutoSyncFailureStatus, now: u64) -> bool {
    !status.retry_suppressed && status.next_retry_at.is_none_or(|deadline| now >= deadline)
}

/// Privacy-safe watcher health exposed through status without project paths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AutoSyncStatus {
    pub active: bool,
    pub backend: &'static str,
    pub events: u64,
    pub reconciliations: u64,
    pub sync_attempts: u64,
    pub publications: u64,
    pub noops: u64,
    pub errors: u64,
    pub last_success_unix_millis: Option<u64>,
    /// Stable credential-safe code for the latest failed automatic index.
    pub last_error_code: Option<&'static str>,
    /// Unix-millisecond timestamp of the latest failed automatic index.
    pub last_failure_at: Option<u64>,
    /// Unix-millisecond deadline for the next automatic retry of this revision.
    pub next_retry_at: Option<u64>,
    /// Automatic failures recorded for the current known or unavailable-revision bucket.
    pub failed_revision_attempts: u8,
    /// True after either the current revision or the cross-revision capacity circuit is exhausted.
    pub retry_suppressed: AutoSyncSuppression,
    /// Consecutive automatic index failures with the same stable error code across revisions.
    pub repeated_failure_attempts: u8,
    /// True after one stable failure code trips the cross-revision automatic-index circuit.
    pub repeated_failure_retry_suppressed: AutoSyncSuppression,
    /// Generation-capacity failures observed since the last successful automatic index.
    pub capacity_failure_attempts: u8,
    /// True after capacity failures trip the cross-revision automatic-index circuit breaker.
    pub capacity_retry_suppressed: AutoSyncSuppression,
    /// Exact project setting that bounded the unresolved capacity failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_limit: Option<&'static str>,
    /// Process boundary whose headroom governs the unresolved capacity failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_scope: Option<&'static str>,
    /// Bounded operator recovery action for an unresolved capacity failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_next_action: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
/// Typed suppression state that preserves the status wire contract as a JSON boolean.
pub(crate) struct AutoSyncSuppression(bool);

impl From<bool> for AutoSyncSuppression {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(crate) enum AutoSyncError {
    #[error("Cartograph native auto-sync watcher is unavailable")]
    WatcherUnavailable,
    #[error("Cartograph project source policy is invalid")]
    InvalidProjectPolicy,
}

enum AutoSyncWatcher {
    Native(RecommendedWatcher),
    Poll(PollWatcher),
}

#[derive(Clone)]
struct WatcherContext {
    root: PathBuf,
    events: mpsc::Sender<()>,
    state: Arc<AutoSyncState>,
    filter: Arc<RwLock<ProjectWatchFilter>>,
}

impl AutoSyncWatcher {
    fn backend(&self) -> &'static str {
        match self {
            Self::Native(watcher) => {
                let _retained_watcher_bytes = std::mem::size_of_val(watcher);
                "native_events"
            }
            Self::Poll(watcher) => {
                let _retained_watcher_bytes = std::mem::size_of_val(watcher);
                "poll_fallback"
            }
        }
    }
}

fn start_watcher(context: &WatcherContext) -> Result<AutoSyncWatcher, AutoSyncError> {
    let native_config = Config::default().with_follow_symlinks(false);
    if let Ok(mut watcher) =
        RecommendedWatcher::new(watcher_handler((*context).clone()), native_config)
        && watcher
            .watch(&context.root, RecursiveMode::Recursive)
            .is_ok()
    {
        return Ok(AutoSyncWatcher::Native(watcher));
    }
    let poll_config = Config::default()
        .with_follow_symlinks(false)
        .with_poll_interval(FALLBACK_POLL_INTERVAL);
    let mut watcher = PollWatcher::new(watcher_handler((*context).clone()), poll_config)
        .map_err(|_| AutoSyncError::WatcherUnavailable)?;
    watcher
        .watch(&context.root, RecursiveMode::Recursive)
        .map_err(|_| AutoSyncError::WatcherUnavailable)?;
    Ok(AutoSyncWatcher::Poll(watcher))
}

fn watcher_handler(context: WatcherContext) -> impl FnMut(notify::Result<Event>) + Send + 'static {
    let WatcherContext {
        root,
        events,
        state,
        filter,
    } = context;
    move |result| match result {
        Ok(event) if event_relevant(&root, &event, &filter) => {
            if event_changes_config(&root, &event) {
                match ProjectWatchFilter::load(&root) {
                    Ok(reloaded) => match filter.write() {
                        Ok(mut current) => *current = reloaded,
                        Err(_) => {
                            state.errors.fetch_add(1, Ordering::AcqRel);
                        }
                    },
                    Err(_) => {
                        state.errors.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
            state.events.fetch_add(1, Ordering::AcqRel);
            let _ = events.try_send(());
        }
        Ok(_) => {}
        Err(_) => {
            state.errors.fetch_add(1, Ordering::AcqRel);
            let _ = events.try_send(());
        }
    }
}

struct AutoSyncTask {
    runtime: Arc<ProjectRuntime>,
    events: mpsc::Receiver<()>,
    cancellation: watch::Receiver<bool>,
    event_debounce: EventDebounce,
    state: Arc<AutoSyncState>,
    startup_reconciliation_enabled: bool,
}

#[derive(Clone, Copy)]
struct EventDebounce {
    quiet_window: Duration,
    maximum_latency: Duration,
}

async fn run_auto_sync(input: AutoSyncTask) {
    let AutoSyncTask {
        runtime,
        mut events,
        mut cancellation,
        event_debounce,
        state,
        startup_reconciliation_enabled,
    } = input;
    let mut reconciliation = tokio::time::interval(RECONCILIATION_INTERVAL);
    reconciliation.set_missed_tick_behavior(MissedTickBehavior::Delay);
    reconciliation.tick().await;
    let startup_reconciliation = tokio::time::sleep(STARTUP_RECONCILIATION_DELAY);
    tokio::pin!(startup_reconciliation);
    let mut startup_reconciliation_pending = startup_reconciliation_enabled;
    loop {
        let retry_at = state.failure_status().next_retry_at;
        let retry_delay = retry_at.map_or(RECONCILIATION_INTERVAL, |deadline| {
            Duration::from_millis(deadline.saturating_sub(unix_millis()))
        });
        tokio::select! {
            biased;
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    break;
                }
            }
            event = events.recv() => {
                if event.is_none() || debounce_events(
                    &mut events,
                    &mut cancellation,
                    event_debounce,
                ).await {
                    break;
                }
                synchronize_after_event(&runtime, &state).await;
            }
            () = &mut startup_reconciliation, if startup_reconciliation_pending => {
                startup_reconciliation_pending = false;
                reconcile(&runtime, &state).await;
            }
            () = tokio::time::sleep(retry_delay), if retry_at.is_some() => {
                reconcile(&runtime, &state).await;
            }
            _ = reconciliation.tick() => {
                reconcile(&runtime, &state).await;
            }
        }
    }
    state.active.store(false, Ordering::Release);
}

async fn reconcile(runtime: &ProjectRuntime, state: &AutoSyncState) {
    state.reconciliations.fetch_add(1, Ordering::AcqRel);
    if state.reconciliation_probe_allowed(unix_millis()) {
        synchronize_if_stale(runtime, state).await;
    }
}

async fn synchronize_if_stale(runtime: &ProjectRuntime, state: &AutoSyncState) {
    let now = unix_millis();
    match runtime.status().await {
        Ok(status)
            if !status.fresh
                && state.automatic_attempt_allowed(&status.live_source_revision, now) =>
        {
            synchronize(runtime, state, Some(status.live_source_revision)).await;
        }
        Ok(status) => {
            if status.fresh {
                state.record_index_success();
            }
        }
        Err(_) => {
            state.record_unknown_failure(&ProjectError::StatusFailed, now);
        }
    }
}

async fn synchronize_after_event(runtime: &ProjectRuntime, state: &AutoSyncState) {
    match state.event_sync_route(unix_millis()) {
        EventSyncRoute::DirectIndex => synchronize(runtime, state, None).await,
        EventSyncRoute::CheckFreshness => synchronize_if_stale(runtime, state).await,
        EventSyncRoute::WaitForRetry => {}
    }
}

async fn debounce_events(
    events: &mut mpsc::Receiver<()>,
    cancellation: &mut watch::Receiver<bool>,
    window: EventDebounce,
) -> bool {
    let deadline = tokio::time::sleep(window.quiet_window);
    let maximum_deadline = tokio::time::sleep(window.maximum_latency);
    tokio::pin!(deadline);
    tokio::pin!(maximum_deadline);
    loop {
        tokio::select! {
            biased;
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    return true;
                }
            }
            () = &mut maximum_deadline => return false,
            () = &mut deadline => return false,
            event = events.recv() => {
                if event.is_none() {
                    return true;
                }
                deadline.as_mut().reset(Instant::now() + window.quiet_window);
            }
        }
    }
}

async fn synchronize(
    runtime: &ProjectRuntime,
    state: &AutoSyncState,
    revision: Option<ContentDigest>,
) {
    state.sync_attempts.fetch_add(1, Ordering::AcqRel);
    match runtime.index(IndexOptions::automatic()).await {
        Ok(report) => {
            if report.published {
                state.publications.fetch_add(1, Ordering::AcqRel);
            } else {
                state.noops.fetch_add(1, Ordering::AcqRel);
            }
            state
                .last_success_unix_millis
                .store(unix_millis(), Ordering::Release);
            state.record_index_success();
        }
        Err(error) => {
            let failed_revision = match revision {
                Some(revision) => Some(revision),
                None => match runtime.status().await {
                    Ok(status) if status.fresh => {
                        state.record_index_success();
                        return;
                    }
                    Ok(status) => Some(status.live_source_revision),
                    Err(_) => None,
                },
            };
            if let Some(failed_revision) = failed_revision {
                state.record_index_failure(failed_revision, &error, unix_millis());
            } else {
                state.record_unknown_failure(&error, unix_millis());
            }
        }
    }
}

fn maximum_debounce_latency(debounce: Duration) -> Duration {
    debounce.max(MAXIMUM_EVENT_COALESCING_LATENCY)
}

fn retry_interval(attempts: u8) -> Duration {
    let exponent = u32::from(attempts.saturating_sub(1)).min(31);
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    INITIAL_RETRY_INTERVAL
        .saturating_mul(multiplier)
        .min(MAXIMUM_RETRY_INTERVAL)
}

const fn project_error_code(error: &ProjectError) -> &'static str {
    match error {
        ProjectError::StatusFailed => "status_failed",
        other => match project_index_failure_code(other) {
            Some(code) => code,
            None => "index_failed",
        },
    }
}

fn event_relevant(root: &Path, event: &Event, filter: &RwLock<ProjectWatchFilter>) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event
        .paths
        .iter()
        .filter_map(|path| path.strip_prefix(root).ok())
        .any(|path| relevant_relative_path(path, filter))
}

fn relevant_relative_path(path: &Path, filter: &RwLock<ProjectWatchFilter>) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.is_empty() {
        return true;
    }
    if normalized == CONFIG_RELATIVE_PATH || normalized == SCIP_OVERLAY_RELATIVE_PATH {
        return true;
    }
    if normalized.starts_with(".cartograph/") {
        return false;
    }
    filter
        .read()
        .map_or(true, |current| current.relevant_relative_path(path))
}

fn event_changes_config(root: &Path, event: &Event) -> bool {
    event.paths.iter().any(|path| {
        path.strip_prefix(root)
            .is_ok_and(|relative| relative == Path::new(CONFIG_RELATIVE_PATH))
    })
}

fn debounce_from_env() -> Duration {
    std::env::var(WATCH_DEBOUNCE_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (MINIMUM_DEBOUNCE_MILLIS..=MAXIMUM_DEBOUNCE_MILLIS).contains(value))
        .map_or(DEFAULT_DEBOUNCE, Duration::from_millis)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(1)
}

const fn nonzero(value: u64) -> Option<u64> {
    if value == 0 { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use std::{env, process, time::SystemTime};

    use cartograph_config::DatabaseSettings;
    use notify::event::AccessKind;
    use sqlx_core::{query::query, sql_str::AssertSqlSafe};

    use super::*;

    #[test]
    fn event_filter_ignores_reads_internal_churn_and_default_excludes() {
        let fixture = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("watch filter tempdir failed: {error}"));
        let root = fixture.path().to_path_buf();
        let filter = RwLock::new(
            ProjectWatchFilter::load(&root)
                .unwrap_or_else(|error| panic!("watch filter load failed: {error}")),
        );
        let event = |kind, relative: &str| Event {
            kind,
            paths: vec![root.join(relative)],
            attrs: notify::event::EventAttributes::new(),
        };
        assert!(!event_relevant(
            &root,
            &event(EventKind::Access(AccessKind::Any), "src/lib.rs"),
            &filter,
        ));
        assert!(!event_relevant(
            &root,
            &event(EventKind::Any, ".cartograph/private-state"),
            &filter,
        ));
        assert!(!event_relevant(
            &root,
            &event(EventKind::Any, "target/debug/cartograph"),
            &filter,
        ));
        assert!(!event_relevant(
            &root,
            &event(EventKind::Any, "node_modules/package/index.js"),
            &filter,
        ));
        assert!(event_relevant(
            &root,
            &event(EventKind::Any, CONFIG_RELATIVE_PATH),
            &filter,
        ));
        assert!(event_changes_config(
            &root,
            &event(EventKind::Any, CONFIG_RELATIVE_PATH)
        ));
        assert!(event_relevant(
            &root,
            &event(EventKind::Any, "src/lib.rs"),
            &filter,
        ));
        assert!(event_relevant(
            &root,
            &event(EventKind::Any, "src"),
            &filter,
        ));
    }

    #[test]
    fn invalid_debounce_values_fall_back_without_panicking() {
        assert_eq!(nonzero(0), None);
        assert_eq!(nonzero(1), Some(1));
        assert!(
            (MINIMUM_DEBOUNCE_MILLIS..=MAXIMUM_DEBOUNCE_MILLIS)
                .contains(&u64::try_from(DEFAULT_DEBOUNCE.as_millis()).unwrap_or_default())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_events_cannot_extend_debounce_past_the_hard_deadline() {
        let quiet_window = Duration::from_millis(100);
        let maximum_latency = Duration::from_millis(250);
        let (sender, mut events) = mpsc::channel(1);
        let (_cancellation_sender, mut cancellation) = watch::channel(false);
        let producer = tokio::spawn(async move {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if sender.send(()).await.is_err() {
                    return;
                }
            }
        });
        let started = Instant::now();

        let cancelled = debounce_events(
            &mut events,
            &mut cancellation,
            EventDebounce {
                quiet_window,
                maximum_latency,
            },
        )
        .await;

        assert!(!cancelled);
        assert_eq!(Instant::now().duration_since(started), maximum_latency);
        assert!(!producer.is_finished());
        producer.abort();
    }

    #[test]
    fn lease_contention_and_parse_churn_retry_after_more_than_five_interruptions() {
        for error in [
            ProjectError::IndexLeaseBusy,
            ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Parse,
                reason: PipelineFailureReason::SourceChangedDuringParse,
            },
        ] {
            let state = AutoSyncState::default();
            let revision = ContentDigest::from_bytes([1; 32]);
            let mut now = 1_000;
            for _ in 0..12 {
                state.record_index_failure(revision.clone(), &error, now);
                let status = state.failure_status();
                assert!(!status.retry_suppressed);
                assert_eq!(status.repeated_failure_attempts, 0);
                assert!(!state.automatic_attempt_allowed(&revision, now));
                let next = status
                    .next_retry_at
                    .unwrap_or_else(|| panic!("recoverable interruption schedules retry"));
                assert!((2_000..=30_000).contains(&(next - now)));
                now = next;
                assert!(state.automatic_attempt_allowed(&revision, now));
            }
        }
    }

    #[test]
    fn recoverable_interruptions_preserve_the_persistent_retry_allowance() {
        let state = AutoSyncState::default();
        let revision = ContentDigest::from_bytes([1; 32]);
        let mut now = 1_000;
        for _ in 0..12 {
            state.record_index_failure(revision.clone(), &ProjectError::IndexLeaseBusy, now);
            now = state
                .failure_status()
                .next_retry_at
                .unwrap_or_else(|| panic!("retry"));
        }
        for attempt in 1..=MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION {
            // Different persistent codes exercise the per-revision breaker independently.
            let failure_stage = if attempt % 2 == 0 {
                PipelineStage::Copy
            } else {
                PipelineStage::Reduce
            };
            state.record_index_failure(
                revision.clone(),
                &ProjectError::IndexStageFailed {
                    stage: failure_stage,
                },
                now,
            );
            let status = state.failure_status();
            assert_eq!(status.repeated_failure_attempts, 1);
            assert_eq!(
                status.retry_suppressed,
                attempt == MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION
            );
            if attempt < MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION {
                if attempt == 1 {
                    assert_eq!(status.next_retry_at, Some(now + 30_000));
                }
                now = status
                    .next_retry_at
                    .unwrap_or_else(|| panic!("persistent retry"));
                state.record_index_failure(
                    revision.clone(),
                    &ProjectError::SourceChangedDuringIndex,
                    now,
                );
                now = state
                    .failure_status()
                    .next_retry_at
                    .unwrap_or_else(|| panic!("transient retry"));
            }
        }
        state.record_index_failure(revision.clone(), &ProjectError::IndexLeaseBusy, now);
        assert!(state.failure_status().retry_suppressed);
        assert!(!state.automatic_attempt_allowed(&revision, u64::MAX));
        assert!(state.automatic_attempt_allowed(&ContentDigest::from_bytes([2; 32]), now));
    }

    #[test]
    fn source_churn_does_not_permanently_disable_automatic_sync() {
        let state = AutoSyncState::default();
        let mut now = 1_000_u64;
        for attempt in 1..=12 {
            let revision = ContentDigest::from_bytes([attempt; 32]);
            assert!(state.automatic_attempt_allowed(&revision, now));
            state.record_index_failure(
                revision.clone(),
                &ProjectError::SourceChangedDuringIndex,
                now,
            );
            let status = state.failure_status();
            assert!(
                !status.retry_suppressed,
                "ordinary source edits must remain recoverable"
            );
            assert_eq!(status.repeated_failure_attempts, 0);
            now = status
                .next_retry_at
                .unwrap_or_else(|| panic!("source churn must schedule recovery"));
            assert!(state.automatic_attempt_allowed(&revision, now));
        }
    }

    #[test]
    fn unchanged_failed_revision_uses_bounded_backoff_and_new_source_resets_it() {
        let state = AutoSyncState::default();
        let first_revision = ContentDigest::from_bytes([1_u8; 32]);
        let next_revision = ContentDigest::from_bytes([2_u8; 32]);
        let mut now = 1_000_u64;

        assert!(state.automatic_attempt_allowed(&first_revision, now));
        state.record_index_failure(
            first_revision.clone(),
            &ProjectError::IndexStageFailed {
                stage: PipelineStage::Reduce,
            },
            now,
        );
        let first = state.failure_status();
        assert_eq!(first.last_error_code, Some("reduce_failed"));
        assert_eq!(first.last_failure_at, Some(now));
        assert_eq!(first.attempts, 1);
        assert!(!first.retry_suppressed);
        assert!(!state.automatic_attempt_allowed(&first_revision, now));
        assert_eq!(state.event_sync_route(now), EventSyncRoute::CheckFreshness);
        now = first
            .next_retry_at
            .unwrap_or_else(|| panic!("first retry deadline was missing"));
        assert!(state.automatic_attempt_allowed(&first_revision, now));

        for expected_attempts in 2..=MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION {
            state.record_index_failure(
                first_revision.clone(),
                &ProjectError::IndexStageFailed {
                    stage: PipelineStage::Copy,
                },
                now,
            );
            let status = state.failure_status();
            assert_eq!(status.attempts, expected_attempts);
            assert_eq!(status.last_error_code, Some("copy_failed"));
            if expected_attempts < MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION {
                now = status
                    .next_retry_at
                    .unwrap_or_else(|| panic!("retry deadline was missing"));
            }
        }
        let exhausted = state.failure_status();
        assert!(exhausted.retry_suppressed);
        assert_eq!(exhausted.next_retry_at, None);
        assert!(!state.automatic_attempt_allowed(&first_revision, u64::MAX));
        assert_eq!(state.event_sync_route(now), EventSyncRoute::CheckFreshness);
        assert!(state.automatic_attempt_allowed(&next_revision, now));

        state.record_index_success();
        assert_eq!(state.failure_status(), AutoSyncFailureStatus::default());
        assert!(state.automatic_attempt_allowed(&first_revision, now));
    }

    #[test]
    fn capacity_failures_trip_a_cross_revision_circuit_breaker() {
        let state = AutoSyncState::default();
        let now = 1_000_u64;
        let capacity_error = ProjectError::IndexStageFailedWithReason {
            stage: PipelineStage::Resolve,
            reason: PipelineFailureReason::GenerationCapacityExceeded,
        };

        for attempt in 1..=MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION {
            let revision = ContentDigest::from_bytes([attempt; 32]);
            assert!(state.automatic_attempt_allowed(&revision, now));
            state.record_index_failure(revision, &capacity_error, now);
            let status = state.failure_status();
            assert_eq!(status.attempts, 1);
            assert_eq!(status.capacity_failure_attempts, attempt);
            assert_eq!(
                status.capacity_retry_suppressed,
                attempt == MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION
            );
        }

        let unseen_revision = ContentDigest::from_bytes([u8::MAX; 32]);
        let exhausted = state.failure_status();
        assert!(exhausted.retry_suppressed);
        assert!(exhausted.capacity_retry_suppressed);
        assert_eq!(exhausted.next_retry_at, None);
        assert!(!state.automatic_attempt_allowed(&unseen_revision, u64::MAX));

        state.record_unknown_failure(&ProjectError::StatusFailed, now);
        let status_gap = state.failure_status();
        assert_eq!(
            status_gap.capacity_failure_attempts,
            MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION
        );
        assert!(status_gap.capacity_retry_suppressed);
        assert!(!state.automatic_attempt_allowed(&unseen_revision, u64::MAX));

        state.record_index_success();
        assert!(state.automatic_attempt_allowed(&unseen_revision, now));
        assert_eq!(state.failure_status(), AutoSyncFailureStatus::default());
    }

    #[test]
    fn repeated_index_failure_code_trips_a_cross_revision_circuit_breaker() {
        let state = AutoSyncState::default();
        let now = 1_000_u64;
        let error = ProjectError::IndexStageFailed {
            stage: PipelineStage::Copy,
        };

        for attempt in 1..=MAXIMUM_REPEATED_AUTOMATIC_FAILURES {
            let revision = ContentDigest::from_bytes([attempt; 32]);
            assert!(state.automatic_attempt_allowed(&revision, now));
            state.record_index_failure(revision, &error, now);
            let status = state.failure_status();
            assert_eq!(status.attempts, 1);
            assert_eq!(status.repeated_failure_attempts, attempt);
            assert_eq!(
                status.repeated_failure_retry_suppressed,
                attempt == MAXIMUM_REPEATED_AUTOMATIC_FAILURES
            );
        }

        let unseen_revision = ContentDigest::from_bytes([u8::MAX; 32]);
        let exhausted = state.failure_status();
        assert!(exhausted.retry_suppressed);
        assert!(exhausted.repeated_failure_retry_suppressed);
        assert_eq!(exhausted.next_retry_at, None);
        assert!(!state.automatic_attempt_allowed(&unseen_revision, u64::MAX));

        state.record_index_success();
        assert!(state.automatic_attempt_allowed(&unseen_revision, now));
        assert_eq!(state.failure_status(), AutoSyncFailureStatus::default());
    }

    #[test]
    fn status_probe_failures_do_not_break_the_repeated_index_failure_sequence() {
        let state = AutoSyncState::default();
        let now = 1_000_u64;
        let error = ProjectError::IndexStageFailed {
            stage: PipelineStage::Copy,
        };

        for attempt in 1..=MAXIMUM_REPEATED_AUTOMATIC_FAILURES {
            if attempt > 1 {
                state.record_unknown_failure(&ProjectError::StatusFailed, now);
                assert_eq!(
                    state.failure_status().last_error_code,
                    Some("status_failed")
                );
            }
            state.record_index_failure(ContentDigest::from_bytes([attempt; 32]), &error, now);
            assert_eq!(state.failure_status().repeated_failure_attempts, attempt);
        }
        assert!(state.failure_status().repeated_failure_retry_suppressed);
    }

    #[test]
    fn distinct_index_failure_codes_do_not_trip_the_cross_revision_circuit() {
        let state = AutoSyncState::default();
        let now = 1_000_u64;

        for attempt in 1..=MAXIMUM_REPEATED_AUTOMATIC_FAILURES.saturating_add(1) {
            let failure_stage = if attempt % 2 == 0 {
                PipelineStage::Copy
            } else {
                PipelineStage::Parse
            };
            let revision = ContentDigest::from_bytes([attempt; 32]);
            assert!(state.automatic_attempt_allowed(&revision, now));
            state.record_index_failure(
                revision,
                &ProjectError::IndexStageFailed {
                    stage: failure_stage,
                },
                now,
            );
            let status = state.failure_status();
            assert_eq!(status.repeated_failure_attempts, 1);
            assert!(!status.repeated_failure_retry_suppressed);
        }
    }

    #[test]
    fn unavailable_revision_failures_throttle_events_without_blocking_recovery_forever() {
        let state = AutoSyncState::default();
        let mut now = 1_000_u64;

        state.record_unknown_failure(&ProjectError::StatusFailed, now);
        let first = state.failure_status();
        assert_eq!(first.last_error_code, Some("status_failed"));
        assert_eq!(first.attempts, 1);
        assert!(!first.retry_suppressed);
        assert_eq!(state.event_sync_route(now), EventSyncRoute::WaitForRetry);
        assert!(!state.reconciliation_probe_allowed(now));

        now = first
            .next_retry_at
            .unwrap_or_else(|| panic!("unknown-revision retry deadline was missing"));
        assert_eq!(state.event_sync_route(now), EventSyncRoute::CheckFreshness);
        assert!(state.reconciliation_probe_allowed(now));

        for expected_attempts in 2..=MAXIMUM_AUTOMATIC_ATTEMPTS_PER_REVISION.saturating_add(1) {
            state.record_unknown_failure(&ProjectError::StatusFailed, now);
            let status = state.failure_status();
            assert_eq!(status.attempts, expected_attempts);
            assert!(!status.retry_suppressed);
            now = status
                .next_retry_at
                .unwrap_or_else(|| panic!("bounded recovery probe deadline was missing"));
        }
        assert_eq!(state.event_sync_route(now), EventSyncRoute::CheckFreshness);

        state.record_index_success();
        assert_eq!(state.failure_status(), AutoSyncFailureStatus::default());
        assert_eq!(state.event_sync_route(now), EventSyncRoute::DirectIndex);
    }

    #[test]
    fn project_error_codes_are_stage_specific_and_credential_safe() {
        let stage_codes = [
            (PipelineStage::Discover, "discover_failed"),
            (PipelineStage::Read, "read_failed"),
            (PipelineStage::Parse, "parse_failed"),
            (PipelineStage::Resolve, "resolve_failed"),
            (PipelineStage::Overlay, "overlay_failed"),
            (PipelineStage::Reduce, "reduce_failed"),
            (PipelineStage::Copy, "copy_failed"),
            (PipelineStage::RelationalMerge, "relational_merge_failed"),
            (PipelineStage::Bm25, "bm25_failed"),
            (PipelineStage::Vector, "vector_failed"),
            (PipelineStage::Publish, "publication_failed"),
        ];
        for (stage, expected) in stage_codes {
            assert_eq!(
                project_error_code(&ProjectError::IndexStageFailed { stage }),
                expected
            );
        }
        assert_eq!(
            project_error_code(&ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Reduce,
                reason: PipelineFailureReason::ReferenceNameTooLong,
            }),
            "reduce_reference_name_too_long"
        );
        assert_eq!(
            project_error_code(&ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Resolve,
                reason: PipelineFailureReason::GenerationCapacityExceeded,
            }),
            "resolve_generation_capacity_exceeded"
        );
        assert_eq!(
            project_error_code(&ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Resolve,
                reason: PipelineFailureReason::DeadlineExceeded,
            }),
            "resolve_deadline_exceeded"
        );
        assert_eq!(
            project_error_code(&ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Resolve,
                reason: PipelineFailureReason::ProgressStalled,
            }),
            "resolve_progress_stalled"
        );
        assert_eq!(
            project_error_code(&ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Parse,
                reason: PipelineFailureReason::GenerationCapacityExceeded,
            }),
            "parse_generation_capacity_exceeded"
        );
        assert_eq!(
            project_error_code(&ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Parse,
                reason: PipelineFailureReason::ExtractionNestingLimitExceeded,
            }),
            "parse_extraction_nesting_limit_exceeded"
        );
        assert_eq!(
            project_error_code(&ProjectError::IndexStageFailedWithReason {
                stage: PipelineStage::Parse,
                reason: PipelineFailureReason::ExtractionOutputLimitExceeded,
            }),
            "parse_extraction_output_limit_exceeded"
        );

        let error_codes = [
            (
                ProjectError::BeginGenerationFailed,
                "generation_start_failed",
            ),
            (ProjectError::SourceScanFailed, "source_scan_failed"),
            (ProjectError::StatusFailed, "status_failed"),
            (ProjectError::IndexLeaseFailed, "lease_failed"),
            (ProjectError::IndexPublicationFailed, "publication_failed"),
            (ProjectError::IndexCleanupFailed, "index_cleanup_failed"),
            (ProjectError::ScipOverlayInvalid, "scip_overlay_invalid"),
            (ProjectError::RequestCancelled, "request_cancelled"),
            (ProjectError::InvalidOptions, "index_failed"),
        ];
        for (error, expected) in error_codes {
            assert_eq!(project_error_code(&error), expected);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
    async fn watcher_recovers_after_a_competing_writer_without_another_edit() {
        use cartograph_db::{LeaseOwner, LeaseRequest, LeaseTarget};
        use cartograph_domain::ProjectOperation;
        let url = env::var("CARTOGRAPH_TEST_DATABASE_URL")
            .unwrap_or_else(|error| panic!("test database: {error}"));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|error| panic!("clock: {error}"))
            .as_nanos();
        let schema = format!("cg_watcher_busy_{}_{stamp}", process::id());
        let guard = cartograph_test_support::TestSchemaGuard::new(&url, &schema)
            .unwrap_or_else(|error| panic!("guard: {error}"));
        let settings = DatabaseSettings::parse(&url, Some("8"), Some("10000"))
            .and_then(|settings| settings.with_schema(&schema))
            .unwrap_or_else(|error| panic!("settings: {error}"));
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("project: {error}"));
        let source = project.path().join("lib.rs");
        std::fs::write(&source, "pub fn original() {}\n")
            .unwrap_or_else(|error| panic!("source: {error}"));
        let runtime = Arc::new(
            ProjectRuntime::connect(project.path(), &settings)
                .await
                .unwrap_or_else(|error| panic!("runtime: {error}")),
        );
        let initial = runtime
            .index(IndexOptions::automatic())
            .await
            .unwrap_or_else(|error| panic!("initial generation: {error}"));
        let lease = runtime
            .database()
            .acquire_lease(LeaseRequest::new(
                LeaseTarget::new(
                    initial.project_id.clone(),
                    ProjectOperation::Migration,
                    None,
                ),
                LeaseOwner::new(process::id(), "watcher-recovery-test"),
                Duration::from_secs(60),
            ))
            .await
            .unwrap_or_else(|error| panic!("competing lease: {error}"));
        let watcher = ProjectAutoSync::start(runtime.clone(), false)
            .unwrap_or_else(|error| panic!("watcher: {error}"));
        std::fs::write(&source, "pub fn replacement() {}\n")
            .unwrap_or_else(|error| panic!("edit: {error}"));
        tokio::time::timeout(Duration::from_secs(15), async {
            while watcher.status().last_error_code != Some("lease_busy") {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("watcher must report live contention: {error}"));
        assert!(
            !runtime
                .status()
                .await
                .unwrap_or_else(|error| panic!("stale status: {error}"))
                .fresh
        );
        runtime
            .database()
            .release_lease(&lease)
            .await
            .unwrap_or_else(|error| panic!("release competitor: {error}"));
        tokio::time::timeout(Duration::from_secs(10), async {
            while watcher.status().publications == 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!("retry timer must recover before the 30 second reconciliation: {error}")
        });
        let status = runtime
            .status()
            .await
            .unwrap_or_else(|error| panic!("recovered status: {error}"));
        assert!(status.fresh);
        assert_ne!(
            status
                .snapshot
                .unwrap_or_else(|| panic!("snapshot"))
                .current
                .unwrap_or_else(|| panic!("current"))
                .generation_id,
            initial.generation_id
        );
        assert_eq!(watcher.status().last_error_code, None);
        drop(watcher);
        drop(runtime);
        guard
            .cleanup()
            .await
            .unwrap_or_else(|error| panic!("cleanup: {error}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
    async fn native_watcher_debounces_an_edit_and_publishes_a_fresh_generation() {
        let url = env::var("CARTOGRAPH_TEST_DATABASE_URL")
            .unwrap_or_else(|_| panic!("live watcher database is not configured"));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let schema = format!("cg_watcher_{}_{}", process::id(), nanos);
        let _schema_guard = cartograph_test_support::TestSchemaGuard::new(&url, schema.clone())
            .unwrap_or_else(|error| panic!("watcher schema guard failed: {error}"));
        let settings = DatabaseSettings::parse(&url, Some("8"), Some("10000"))
            .and_then(|settings| settings.with_schema(&schema))
            .unwrap_or_else(|error| panic!("watcher settings failed: {error}"));
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let source = project.path().join("service.rs");
        std::fs::write(&source, "pub fn value() -> u32 { 1 }\n")
            .unwrap_or_else(|error| panic!("watcher fixture failed: {error}"));
        let runtime = Arc::new(
            ProjectRuntime::connect(project.path(), &settings)
                .await
                .unwrap_or_else(|error| panic!("watcher runtime failed: {error}")),
        );
        let first = runtime
            .index(IndexOptions::default().with_history_refresh(false))
            .await
            .unwrap_or_else(|error| panic!("watcher initial index failed: {error}"));
        let watcher = ProjectAutoSync::start(runtime.clone(), true)
            .unwrap_or_else(|error| panic!("watcher start failed: {error}"));
        std::fs::write(&source, "pub fn value() -> u32 { 2 }\n")
            .unwrap_or_else(|error| panic!("watcher edit failed: {error}"));
        let refreshed = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = runtime
                    .status()
                    .await
                    .unwrap_or_else(|error| panic!("watcher status failed: {error}"));
                if status.fresh
                    && status.snapshot.as_ref().and_then(|snapshot| {
                        snapshot
                            .current
                            .as_ref()
                            .map(|current| current.generation_id.clone())
                    }) != Some(first.generation_id.clone())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            refreshed.is_ok(),
            "watcher did not refresh within its bound"
        );
        let watcher_status = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = watcher.status();
                if status.publications > 0 || status.noops > 0 || status.errors > 0 {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("watcher did not record its terminal outcome within its bound"));
        assert!(watcher_status.active);
        assert!(watcher_status.events >= 1 || watcher_status.reconciliations >= 1);
        assert!(watcher_status.sync_attempts >= 1);
        assert!(
            watcher_status.publications >= 1,
            "watcher did not record its publication: {watcher_status:?}"
        );
        assert_eq!(watcher_status.errors, 0);
        drop(watcher);
        drop(runtime);

        let pool = cartograph_db::connect(&settings)
            .await
            .unwrap_or_else(|error| panic!("watcher cleanup connection failed: {error}"));
        query(AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
        )))
        .execute(&pool)
        .await
        .unwrap_or_else(|_| panic!("watcher cleanup failed"));
        pool.close().await;
    }
}
