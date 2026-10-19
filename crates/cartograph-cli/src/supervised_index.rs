//! Cooperative cancellation and parent supervision for `cartograph index`.
//!
//! Once its cancellable index request starts, an index run turns its first
//! SIGINT/SIGTERM (Ctrl-C on Windows) into a cooperative
//! [`ProjectCancellation`]: the indexer supervisor then fails its staging
//! generation and releases its exact project lease instead of leaving both
//! behind until the lease expires. Before that (while the run resolves its
//! database, connects, and migrates the schema), on a second interrupt, and
//! after the request ended, an interrupt ends the process as the signal's
//! default disposition does, even one the process inherited as ignored. A
//! cancelled request reports `request_cancelled` as its primary failure. It
//! adds the secondary `cleanup_failure` unless PostgreSQL confirms the cleanup
//! of what that request itself held. Other writers' leases and generations on
//! the same project are never part of that proof. MCP admin index jobs run the
//! same direct path under their job cancellation, so both surfaces report a
//! cancelled index's cleanup identically.
//!
//! The hidden `--supervised` mode is the child that `upgrade --apply` runs.
//! Its parent holds stdin open and closes it to request the same cooperative
//! cancellation, reads one compact JSON progress line on stderr whenever the
//! observable work changes (from the child's start, including bounded
//! liveness while it connects and migrates), and relies on this module
//! waiting (within [`SUPERVISED_WRITER_WAIT`]) for another live writer on the
//! project instead of failing immediately with `lease_busy`, including a
//! writer that started while this child scanned the checkout and now blocks
//! its attempt. `sync-if-dirty` waits on the same project leases through
//! [`WriterCollisionWait`] after its own `lease_busy` attempts.

use std::{
    io::{Read, Write as _},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use cartograph_agent::{
    IndexFailure, IndexOptions, IndexReport, PipelineStage, ProjectCancellation, ProjectError,
    ProjectRuntime, SupervisorState, SupervisorStatus,
};
use cartograph_domain::ProjectId;
use serde::Serialize;
use tokio::time::{Instant, MissedTickBehavior};

/// Longest a supervised index waits for other live writers (for example an
/// MCP server's auto-sync) to release the project before it reports
/// `lease_busy`. Each writer's own supervisor bounds its run.
pub(crate) const SUPERVISED_WRITER_WAIT: Duration = Duration::from_secs(300);
/// Longest a supervised child reports liveness while it starts: resolving
/// its database settings, connecting, and applying schema migrations and
/// startup search-relation repairs, each statement of which runs under the
/// session statement timeout. A startup still running after this stops
/// changing its progress line, so the parent's inactivity bound then stops it
/// like any other child that stopped reporting.
pub(crate) const SUPERVISED_STARTUP_ALLOWANCE: Duration = Duration::from_secs(300);
/// First pause after a `lease_busy` attempt that no live lease explained,
/// such as a project lock held past the bounded staging-preflight wait by an
/// operation without a lease, or a schema-maintenance lock; it doubles up to
/// [`MAXIMUM_COLLISION_BACKOFF`] because every collision costs a source scan.
/// A collision with a live writer instead waits for that writer's lease.
const INITIAL_COLLISION_BACKOFF: Duration = Duration::from_millis(100);
/// Longest pause between acquisition collisions.
const MAXIMUM_COLLISION_BACKOFF: Duration = Duration::from_secs(2);
/// Interval between progress observations; a line is written only on change.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Scratch buffer for discarding unexpected bytes written to a supervised stdin.
const STDIN_DRAIN_BYTES: usize = 256;
/// Shell convention for the exit status of a process ended by signal `N`:
/// this base plus `N`. Used only if re-raising the signal failed to end it.
#[cfg(unix)]
const SIGNAL_EXIT_STATUS_BASE: i32 = 128;
/// `STATUS_CONTROL_C_EXIT`: the status the default console handler exits
/// with on Ctrl-C.
#[cfg(windows)]
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// How one `cartograph index` request is driven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexSupervision {
    /// An operator or script runs the index directly.
    Direct,
    /// `upgrade --apply` supervises this process through stdin and stderr.
    Supervised,
}

/// One cancellable index request.
pub(crate) struct CancellableIndex<'request> {
    /// Connected project runtime.
    pub(crate) runtime: &'request ProjectRuntime,
    /// Index policy, already including the supervised-mode policy.
    pub(crate) options: IndexOptions,
    /// Signal shared with the interrupt forwarder and stdin watcher.
    pub(crate) cancellation: ProjectCancellation,
    /// Direct or supervised mode.
    pub(crate) supervision: IndexSupervision,
    /// The process's interrupt forwarding, told when the request starts and
    /// ends. `None` for an MCP admin job: its job cancellation stops it, and
    /// the server process keeps its own signal handling.
    pub(crate) interrupts: Option<&'request InterruptForwarding>,
}

/// Await the startup of one index request: resolving its database settings,
/// connecting, and applying schema migrations, before any cancellable index
/// work exists.
///
/// A supervised child reports a `starting` progress line from its first
/// moment and changes it every [`PROGRESS_POLL_INTERVAL`] for at most
/// [`SUPERVISED_STARTUP_ALLOWANCE`], so its parent does not mistake a long
/// migration for a stalled child, while a startup that outlives the allowance
/// falls back under the parent's inactivity bound. A direct index reports
/// nothing.
pub(crate) async fn start_up<T>(
    supervision: IndexSupervision,
    startup: impl Future<Output = T>,
) -> T {
    match supervision {
        IndexSupervision::Direct => startup.await,
        IndexSupervision::Supervised => {
            with_startup_liveness(startup, emit_progress::<StartupProgress>).await
        }
    }
}

/// Await `startup` while `emit` reports its bounded liveness.
async fn with_startup_liveness<T>(
    startup: impl Future<Output = T>,
    mut emit: impl FnMut(&StartupProgress),
) -> T {
    tokio::pin!(startup);
    tokio::select! {
        biased;
        output = &mut startup => return output,
        () = report_startup_liveness(&mut emit) => {}
    }
    startup.await
}

/// Report one changed startup line now and after every
/// [`PROGRESS_POLL_INTERVAL`] until [`SUPERVISED_STARTUP_ALLOWANCE`] elapsed.
async fn report_startup_liveness(emit: &mut impl FnMut(&StartupProgress)) {
    let allowance_ends = Instant::now() + SUPERVISED_STARTUP_ALLOWANCE;
    let mut ticker = tokio::time::interval(PROGRESS_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut startup_heartbeats = 0;
    loop {
        ticker.tick().await;
        if Instant::now() >= allowance_ends {
            return;
        }
        emit(&StartupProgress {
            phase: SupervisedPhase::Starting,
            startup_heartbeats,
        });
        startup_heartbeats += 1;
    }
}

/// Run one index request under its cancellation signal.
///
/// For a request that carries the process's interrupt forwarding, interrupts
/// become cooperative stops only while the request runs (see
/// [`InterruptForwarding`]); before it starts, nothing would act on a
/// cancellation yet, so they end the process.
/// A failure keeps the primary error and any cleanup failure. Cancellation
/// drains the owned native work before releasing the project writer guard.
pub(crate) fn run_cancellable_index(
    request: CancellableIndex<'_>,
) -> impl std::future::Future<Output = Result<IndexReport, IndexFailure>> + Send + '_ {
    Box::pin(async move {
        let interrupts = request.interrupts;
        if let Some(interrupts) = interrupts {
            interrupts.request_started();
        }
        let result = Box::pin(run_request(request)).await;
        if let Some(interrupts) = interrupts {
            interrupts.request_finished();
        }
        result
    })
}

/// Run the request in its direct or supervised mode.
async fn run_request(request: CancellableIndex<'_>) -> Result<IndexReport, IndexFailure> {
    let CancellableIndex {
        runtime,
        options,
        cancellation,
        supervision,
        interrupts: _,
    } = request;
    match supervision {
        IndexSupervision::Direct => {
            Box::pin(with_cleanup_proof(
                runtime,
                &cancellation,
                runtime.index_with_cancellation_detail(options, cancellation.clone()),
            ))
            .await
        }
        IndexSupervision::Supervised => {
            Box::pin(run_supervised_index(SupervisedIndex {
                runtime,
                options,
                cancellation: &cancellation,
            }))
            .await
        }
    }
}

/// Process-interrupt forwarding for one index request and everything the
/// process does after it.
///
/// The listeners are installed synchronously before any work starts and are
/// never removed: tokio never restores a signal's default disposition, so
/// removing them would make every later interrupt silently ignored, including
/// during the remaining steps of a caller such as `install`. Only while the
/// request runs (between [`Self::request_started`] and
/// [`Self::request_finished`]) does the first interrupt cancel it
/// cooperatively; a second one ends the process. Before the request starts
/// (settings, connection, schema migrations), when nothing would act on a
/// cancellation, and after it ended, any interrupt ends the process. Ending the
/// process emulates the default disposition (see [`end_process_by`]), even
/// for a signal the process inherited as ignored, as a non-interactive shell's
/// background job does with SIGINT. A process runs at most one index request,
/// so one forwarding is installed.
pub(crate) struct InterruptForwarding {
    phase: Arc<RequestPhase>,
}

impl InterruptForwarding {
    /// Install the listeners and forward interrupts into `cancellation`;
    /// `announce` tells an interactive operator that the stop was accepted.
    /// When no listener can be installed, default signal behavior remains.
    /// The forwarding task runs until the runtime shuts down.
    pub(crate) fn install(cancellation: ProjectCancellation, announce: bool) -> Self {
        let phase = Arc::new(RequestPhase::default());
        if let Some(interrupts) = Interrupts::listen() {
            drop(tokio::spawn(forward_interrupts(
                interrupts,
                InterruptRoute {
                    cancellation,
                    announce,
                    phase: phase.clone(),
                },
            )));
        }
        Self { phase }
    }

    /// The cancellable index request started; an interrupt now stops it
    /// cooperatively.
    pub(crate) fn request_started(&self) {
        self.phase.enter(RequestPhase::RUNNING);
    }

    /// The index request ended; any later interrupt ends the process as the
    /// default disposition would have.
    pub(crate) fn request_finished(&self) {
        self.phase.enter(RequestPhase::ENDED);
    }
}

/// How far the process's one index request got, as interrupts see it.
///
/// One word holds the phase, written by the request task and read by the
/// interrupt forwarder, so the forwarder never sees a half-updated pair of
/// flags, and it only ever moves forward.
#[derive(Default)]
struct RequestPhase(AtomicU8);

impl RequestPhase {
    /// The cancellable request runs.
    const RUNNING: u8 = 1;
    /// The request ended.
    const ENDED: u8 = 2;

    /// Move to `phase` unless the request already got further.
    fn enter(&self, phase: u8) {
        self.0.fetch_max(phase, Ordering::AcqRel);
    }

    /// Whether an interrupt now becomes a cooperative stop: only while the
    /// request runs.
    fn running(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::RUNNING
    }
}

/// Await the native owner through cancellation and preserve its primary and cleanup outcomes.
/// Native staging rollback and worker reaping complete inside the runtime future.
pub(crate) async fn with_cleanup_proof<Report>(
    _runtime: &ProjectRuntime,
    _cancellation: &ProjectCancellation,
    request: impl Future<Output = Result<Report, IndexFailure>>,
) -> Result<Report, IndexFailure> {
    request.await
}

/// Treat EOF or a read failure on stdin as a cooperative cancellation request.
///
/// A dedicated thread performs the blocking read: tokio's stdin cannot be
/// cancelled and would hold runtime shutdown open until the parent closed the
/// pipe. The thread is deliberately not joined; it ends with the process.
/// Because the read also returns when the parent process dies, an orphaned
/// supervised index still releases its lease cooperatively.
///
/// # Errors
///
/// Returns an error when the watcher thread cannot be started; a supervised
/// run must not start without its cancellation channel.
pub(crate) fn cancel_on_stdin_eof(cancellation: ProjectCancellation) -> Result<(), String> {
    std::thread::Builder::new()
        .name("cartograph-index-stdin".to_owned())
        .spawn(move || {
            drain_until_eof(&mut std::io::stdin().lock());
            cancellation.cancel();
        })
        .map(|_detached| ())
        .map_err(|_| "could not watch stdin for supervised index cancellation".to_owned())
}

/// Discard input until EOF; any read failure other than an interrupted call
/// also ends the wait, because a broken channel must still cancel.
fn drain_until_eof(input: &mut impl Read) {
    let mut buffer = [0_u8; STDIN_DRAIN_BYTES];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Where an interrupt goes while the command runs.
struct InterruptRoute {
    cancellation: ProjectCancellation,
    announce: bool,
    phase: Arc<RequestPhase>,
}

async fn forward_interrupts(mut interrupts: Interrupts, route: InterruptRoute) {
    let Some(first) = interrupts.next().await else {
        return;
    };
    if !route.phase.running() {
        end_process_by(first);
    }
    route.cancellation.cancel();
    if route.announce {
        eprintln!(
            "cartograph: stopping the index cooperatively so it releases its project lease; interrupt again to exit immediately"
        );
    }
    if let Some(second) = interrupts.next().await {
        end_process_by(second);
    }
}

/// One interrupt the process received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptSignal {
    /// `SIGINT`, such as a terminal's Ctrl-C.
    #[cfg(unix)]
    Interrupt,
    /// `SIGTERM`, such as a service manager's stop request.
    #[cfg(unix)]
    Terminate,
    /// A console Ctrl-C.
    #[cfg(windows)]
    CtrlC,
}

/// End the process the way `interrupt`'s default disposition would have.
///
/// On Unix that is death by the received signal, not an exit status, so a
/// parent shell sees the signal and stops its loop or script too: the
/// listener's handler is replaced by `SIG_DFL`, the signal unblocked and
/// raised again. On Windows the default console handler exits with
/// `STATUS_CONTROL_C_EXIT`.
fn end_process_by(interrupt: InterruptSignal) -> ! {
    match interrupt {
        #[cfg(unix)]
        InterruptSignal::Interrupt => raise_with_default_disposition(signal_hook::consts::SIGINT),
        #[cfg(unix)]
        InterruptSignal::Terminate => raise_with_default_disposition(signal_hook::consts::SIGTERM),
        #[cfg(windows)]
        InterruptSignal::CtrlC => std::process::exit(STATUS_CONTROL_C_EXIT.cast_signed()),
    }
}

/// Restore `signal`'s default disposition and raise it. The emulation itself
/// aborts if the raised signal returns, so it returns only for a signal it
/// does not know, which then exits with the shell's status for that signal.
#[cfg(unix)]
fn raise_with_default_disposition(signal: std::ffi::c_int) -> ! {
    let _unknown_signal = signal_hook::low_level::emulate_default_handler(signal);
    std::process::exit(SIGNAL_EXIT_STATUS_BASE + signal)
}

/// SIGINT and SIGTERM listeners; either may be unavailable.
#[cfg(unix)]
struct Interrupts {
    interrupt: Option<tokio::signal::unix::Signal>,
    terminate: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl Interrupts {
    /// Install every listener that registers. A signal whose listener failed
    /// keeps its default behavior; `None` when neither registered.
    fn listen() -> Option<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let interrupts = Self {
            interrupt: signal(SignalKind::interrupt()).ok(),
            terminate: signal(SignalKind::terminate()).ok(),
        };
        (interrupts.interrupt.is_some() || interrupts.terminate.is_some()).then_some(interrupts)
    }

    /// Wait for the next interrupt; `None` once a signal stream closed.
    async fn next(&mut self) -> Option<InterruptSignal> {
        tokio::select! {
            received = next_signal(self.interrupt.as_mut()) => {
                received.then_some(InterruptSignal::Interrupt)
            }
            received = next_signal(self.terminate.as_mut()) => {
                received.then_some(InterruptSignal::Terminate)
            }
        }
    }
}

/// Wait on one optional listener; an absent listener never fires.
#[cfg(unix)]
async fn next_signal(listener: Option<&mut tokio::signal::unix::Signal>) -> bool {
    match listener {
        Some(listener) => listener.recv().await.is_some(),
        None => std::future::pending().await,
    }
}

/// Console Ctrl-C listener.
#[cfg(windows)]
struct Interrupts {
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl Interrupts {
    /// Install the listener; `None` keeps the default console behavior.
    fn listen() -> Option<Self> {
        Some(Self {
            ctrl_c: tokio::signal::windows::ctrl_c().ok()?,
        })
    }

    /// Wait for the next interrupt; `None` once the signal stream closed.
    async fn next(&mut self) -> Option<InterruptSignal> {
        self.ctrl_c.recv().await.map(|()| InterruptSignal::CtrlC)
    }
}

/// No interrupt listener exists on this platform.
#[cfg(not(any(unix, windows)))]
enum Interrupts {}

#[cfg(not(any(unix, windows)))]
impl Interrupts {
    const fn listen() -> Option<Self> {
        None
    }

    async fn next(&mut self) -> Option<InterruptSignal> {
        match *self {}
    }
}

struct SupervisedIndex<'request> {
    runtime: &'request ProjectRuntime,
    options: IndexOptions,
    cancellation: &'request ProjectCancellation,
}

/// Index after other writers finish while reporting progress until it ends.
async fn run_supervised_index(request: SupervisedIndex<'_>) -> Result<IndexReport, IndexFailure> {
    let SupervisedIndex {
        runtime,
        options,
        cancellation,
    } = request;
    let progress = SupervisedProgress::default();
    let wait = WriterWait {
        runtime,
        progress: &progress,
        cancellation,
        deadline: Instant::now() + SUPERVISED_WRITER_WAIT,
    };
    let work = index_after_other_writers(&wait, options);
    tokio::pin!(work);
    let reporter = report_progress(runtime, &progress, cancellation);
    tokio::pin!(reporter);
    loop {
        tokio::select! {
            biased;
            result = &mut work => return result,
            () = &mut reporter => {}
        }
    }
}

async fn index_after_other_writers(
    wait: &WriterWait<'_>,
    options: IndexOptions,
) -> Result<IndexReport, IndexFailure> {
    attempt_until_final(wait, options).await
}

async fn attempt_until_final(
    wait: &WriterWait<'_>,
    options: IndexOptions,
) -> Result<IndexReport, IndexFailure> {
    let mut backoff = INITIAL_COLLISION_BACKOFF;
    loop {
        if wait.cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled.into());
        }
        if Instant::now() >= wait.deadline {
            return Err(ProjectError::IndexLeaseBusy.into());
        }
        wait.progress.attempts.fetch_add(1, Ordering::Relaxed);
        wait.progress.indexing.store(true, Ordering::Relaxed);
        let result = wait
            .runtime
            .index_with_cancellation_detail(options.clone(), wait.cancellation.clone())
            .await;
        if !refused_by_another_writer(&result) || wait.cancellation.is_cancelled() {
            return result;
        }
        wait.progress.indexing.store(false, Ordering::Relaxed);
        within_wait(wait, tokio::time::sleep(backoff)).await?;
        backoff = backoff.saturating_mul(2).min(MAXIMUM_COLLISION_BACKOFF);
    }
}

/// Whether another project writer refused the attempt.
///
/// The native project owner reports `lease_busy` when another index holds its
/// publication guard. Storage faults remain errors rather than contention.
const fn refused_by_another_writer(result: &Result<IndexReport, IndexFailure>) -> bool {
    match result {
        Err(failure) => matches!(failure.error(), ProjectError::IndexLeaseBusy),
        Ok(_) => false,
    }
}

struct WriterWait<'wait> {
    runtime: &'wait ProjectRuntime,
    progress: &'wait SupervisedProgress,
    cancellation: &'wait ProjectCancellation,
    deadline: Instant,
}

/// `sync-if-dirty`'s bounded wait between its `lease_busy` attempts; a
/// deadline that passes first reports the retryable `lease_busy`.
///
/// Native writer collisions pause from [`INITIAL_COLLISION_BACKOFF`], doubling
/// up to [`MAXIMUM_COLLISION_BACKOFF`], under the original overall deadline.
/// This path does not start another source scan while backing off.
pub(crate) struct WriterCollisionWait {
    deadline: Instant,
    backoff: Duration,
}

impl WriterCollisionWait {
    /// A wait that reports `lease_busy` once `deadline` passes.
    pub(crate) const fn until(deadline: Instant) -> Self {
        Self {
            deadline,
            backoff: INITIAL_COLLISION_BACKOFF,
        }
    }

    /// When the whole wait ends.
    pub(crate) const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) async fn after_collision(
        &mut self,
        _runtime: &ProjectRuntime,
        _project_id: &ProjectId,
    ) -> Result<(), ProjectError> {
        if Instant::now() >= self.deadline {
            return Err(ProjectError::IndexLeaseBusy);
        }
        tokio::time::sleep_until((Instant::now() + self.backoff).min(self.deadline)).await;
        if Instant::now() >= self.deadline {
            return Err(ProjectError::IndexLeaseBusy);
        }
        self.backoff = self
            .backoff
            .saturating_mul(2)
            .min(MAXIMUM_COLLISION_BACKOFF);
        Ok(())
    }
}

/// Run `work` unless cancellation (`request_cancelled`) or the wait deadline
/// (`lease_busy`) comes first.
async fn within_wait<T>(
    wait: &WriterWait<'_>,
    work: impl Future<Output = T>,
) -> Result<T, ProjectError> {
    tokio::select! {
        biased;
        () = wait.cancellation.cancelled() => Err(ProjectError::RequestCancelled),
        () = tokio::time::sleep_until(wait.deadline) => Err(ProjectError::IndexLeaseBusy),
        output = work => Ok(output),
    }
}

/// Counters the supervised loop updates and the progress reporter reads.
#[derive(Default)]
struct SupervisedProgress {
    indexing: AtomicBool,
    attempts: AtomicU32,
    writer_heartbeats: AtomicU64,
}

impl SupervisedProgress {
    fn snapshot(&self, observation: ProgressObservation<'_>) -> ProgressSnapshot {
        let status = observation.status;
        ProgressSnapshot {
            phase: if self.indexing.load(Ordering::Relaxed) {
                SupervisedPhase::Indexing
            } else {
                SupervisedPhase::WaitingForWriter
            },
            attempt: self.attempts.load(Ordering::Relaxed),
            writer_heartbeats: self.writer_heartbeats.load(Ordering::Relaxed),
            source_scans: observation.source_scans,
            scan_checkpoints: observation.scan_checkpoints,
            state: status.map(SupervisorStatus::state),
            stage: status.and_then(SupervisorStatus::stage),
            completed_items: status.map_or(0, SupervisorStatus::completed_items),
            completed_bytes: status.map_or(0, SupervisorStatus::completed_bytes),
            heartbeats: status.map_or(0, SupervisorStatus::heartbeat_count),
        }
    }
}

/// Runtime evidence read alongside the shared counters.
#[derive(Clone, Copy)]
struct ProgressObservation<'status> {
    status: Option<&'status SupervisorStatus>,
    /// Source scans started by the runtime.
    source_scans: u64,
    /// Discovery and hashing checkpoints passed inside those scans.
    scan_checkpoints: u64,
}

/// Coarse step of a supervised index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SupervisedPhase {
    /// The child resolves its database settings, connects, and applies
    /// schema migrations; no index attempt has started.
    Starting,
    /// Another live writer holds a lease on the project.
    WaitingForWriter,
    /// An index attempt is preparing, building, or finalizing.
    Indexing,
}

/// One observation of the child's work; a line is written only on change.
///
/// Work counters (stage, items, bytes, started source scans and the
/// discovery/hashing checkpoints inside them) show direct advancement,
/// including during the unsupervised source scans before and after the
/// build. `heartbeats` counts the supervisor's lease renewals: while they
/// continue, its own watchdog is running and stops the build itself after 10
/// minutes without stage or database-preparation progress, so the parent
/// need only detect a child whose renewals and counters all stopped.
/// `writer_heartbeats` shows another writer is alive during the bounded wait.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ProgressSnapshot {
    phase: SupervisedPhase,
    attempt: u32,
    writer_heartbeats: u64,
    source_scans: u64,
    scan_checkpoints: u64,
    state: Option<SupervisorState>,
    stage: Option<PipelineStage>,
    completed_items: u64,
    completed_bytes: u64,
    heartbeats: u64,
}

/// One liveness observation while the child starts. Startup reports no
/// work counters, so `startup_heartbeats` changes every line until the
/// bounded allowance ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct StartupProgress {
    /// Always [`SupervisedPhase::Starting`].
    phase: SupervisedPhase,
    /// Startup lines written before this one.
    startup_heartbeats: u64,
}

/// The `{"progress": ...}` envelope the parent reads from stderr.
#[derive(Serialize)]
struct ProgressLine<'snapshot, Snapshot> {
    progress: &'snapshot Snapshot,
}

/// Write a progress line whenever the observation changes. Never returns;
/// the caller drops it when the index request ends.
async fn report_progress(
    runtime: &ProjectRuntime,
    progress: &SupervisedProgress,
    cancellation: &ProjectCancellation,
) {
    let mut ticker = tokio::time::interval(PROGRESS_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last = None;
    loop {
        ticker.tick().await;
        let status = cancellation.index_progress_status().await;
        let snapshot = progress.snapshot(ProgressObservation {
            status: status.as_ref(),
            source_scans: runtime.source_scan_observations(),
            scan_checkpoints: runtime.source_scan_checkpoints(),
        });
        if last.as_ref() != Some(&snapshot) {
            emit_progress(&snapshot);
            last = Some(snapshot);
        }
    }
}

/// Write one complete line in a single call so it cannot interleave.
fn emit_progress<Snapshot: Serialize>(snapshot: &Snapshot) {
    let Ok(mut line) = serde_json::to_vec(&ProgressLine { progress: snapshot }) else {
        return;
    };
    line.push(b'\n');
    let _ = std::io::stderr().lock().write_all(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: ProgressObservation<'static> = ProgressObservation {
        status: None,
        source_scans: 0,
        scan_checkpoints: 0,
    };

    #[test]
    fn progress_lines_change_only_when_work_or_liveness_advances() {
        let progress = SupervisedProgress::default();
        let waiting = progress.snapshot(IDLE);
        assert_eq!(waiting, progress.snapshot(IDLE));
        let line = serde_json::to_value(ProgressLine { progress: &waiting })
            .unwrap_or_else(|error| panic!("progress line did not serialize: {error}"));
        assert_eq!(line["progress"]["phase"], "waiting_for_writer");
        assert_eq!(line["progress"]["state"], serde_json::Value::Null);

        progress.writer_heartbeats.fetch_add(1, Ordering::Relaxed);
        let renewed = progress.snapshot(IDLE);
        assert_ne!(
            renewed, waiting,
            "another writer's renewal shows the wait is live"
        );

        progress.indexing.store(true, Ordering::Relaxed);
        progress.attempts.fetch_add(1, Ordering::Relaxed);
        let indexing = progress.snapshot(IDLE);
        assert_ne!(indexing, renewed);
        assert_eq!(indexing.phase, SupervisedPhase::Indexing);
        assert_eq!(indexing.attempt, 1);

        let scanning = progress.snapshot(ProgressObservation {
            source_scans: 1,
            ..IDLE
        });
        assert_ne!(scanning, indexing, "starting a source scan is progress");
        let hashing = progress.snapshot(ProgressObservation {
            source_scans: 1,
            scan_checkpoints: 1,
            ..IDLE
        });
        assert_ne!(
            hashing, scanning,
            "files hashed inside one long scan are progress"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_startup_reports_changing_liveness_only_within_its_allowance() {
        // Longer than the allowance plus the parent's whole inactivity bound.
        let startup_duration = SUPERVISED_STARTUP_ALLOWANCE + Duration::from_mins(20);
        let started = Instant::now();
        let mut lines = Vec::new();
        let connected = with_startup_liveness(
            async {
                tokio::time::sleep(startup_duration).await;
                "connected"
            },
            |line: &StartupProgress| lines.push((started.elapsed(), *line)),
        )
        .await;
        assert_eq!(connected, "connected", "the startup itself is never cut");
        assert_eq!(started.elapsed(), startup_duration);

        // The first line is written at once, and every later one changes
        // within one poll interval, so the parent's inactivity bound never
        // sees a gap while the allowance lasts.
        assert_eq!(lines.first().map(|(at, _)| *at), Some(Duration::ZERO));
        assert!(
            lines
                .array_windows()
                .all(|[(earlier, previous), (later, next)]| {
                    previous != next
                        && later
                            .checked_sub(*earlier)
                            .is_some_and(|gap| gap <= PROGRESS_POLL_INTERVAL)
                })
        );
        // Liveness covers the whole allowance and stops with it, leaving a
        // startup that runs longer to the parent's inactivity bound.
        let (last, _) = lines
            .last()
            .copied()
            .unwrap_or_else(|| panic!("a slow startup reported no liveness"));
        assert!(last < SUPERVISED_STARTUP_ALLOWANCE);
        assert!(last + PROGRESS_POLL_INTERVAL >= SUPERVISED_STARTUP_ALLOWANCE);

        let line = serde_json::to_value(ProgressLine {
            progress: &lines[1].1,
        })
        .unwrap_or_else(|error| panic!("startup line did not serialize: {error}"));
        assert_eq!(
            line,
            serde_json::json!({"progress": {"phase": "starting", "startup_heartbeats": 1}})
        );
    }

    /// Bound of each live wait that another operation's lease outlasts, and
    /// how long the competing writer holds its lease before releasing it.
    /// Bound of the wait that the competing writer's release ends first.
    /// A broken stdin: interrupted once, then failing permanently.
    struct BrokenStdin {
        reads: u8,
    }

    impl Read for BrokenStdin {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            let kind = if self.reads == 1 {
                std::io::ErrorKind::Interrupted
            } else {
                std::io::ErrorKind::BrokenPipe
            };
            Err(std::io::Error::from(kind))
        }
    }

    #[test]
    fn stdin_watcher_returns_on_eof_and_on_read_failure() {
        let mut eof = std::io::Cursor::new(b"ignored parent bytes".to_vec());
        drain_until_eof(&mut eof);
        assert_eq!(eof.position(), 20);

        // A broken channel must end the wait (and so cancel) instead of
        // spinning, while an interrupted read is retried.
        let mut broken = BrokenStdin { reads: 0 };
        drain_until_eof(&mut broken);
        assert_eq!(broken.reads, 2);
    }
}

/// Interrupt exits observed from outside: the test binary re-executes itself
/// as a child that owns the signal listeners, as `cartograph index` does.
#[cfg(all(test, unix))]
mod interrupt_exit_tests {
    use std::{os::unix::process::ExitStatusExt as _, process::Command};

    use super::*;

    /// Selects the child scenario of a re-executed test binary.
    const SCENARIO_ENV: &str = "CARTOGRAPH_TEST_INTERRUPT_SCENARIO";
    /// The request already ended when `SIGINT` arrives.
    const AFTER_REQUEST: &str = "after-request";
    /// A second `SIGTERM` arrives while the request is still running.
    const SECOND_INTERRUPT: &str = "second-interrupt";
    /// Longest a child may survive the interrupt that must end it.
    const CHILD_BOUND: Duration = Duration::from_secs(20);
    /// Interval between checks that the first interrupt was forwarded.
    const FORWARD_POLL: Duration = Duration::from_millis(10);

    /// Send `signal` (a `kill` option such as `-INT`) to this process.
    fn signal_self(signal: &str) {
        let sent = Command::new("kill")
            .args([signal, &std::process::id().to_string()])
            .status()
            .unwrap_or_else(|error| panic!("kill did not start: {error}"));
        assert!(sent.success());
    }

    /// Child side: install the forwarding, deliver the scenario's
    /// interrupts, and outlive them only if the forwarding fails to end the
    /// process.
    async fn interrupted_child(scenario: &str) {
        let cancellation = ProjectCancellation::new();
        let forwarding = InterruptForwarding::install(cancellation.clone(), false);
        forwarding.request_started();
        if scenario == AFTER_REQUEST {
            forwarding.request_finished();
            signal_self("-INT");
        } else {
            signal_self("-TERM");
            // Signals are coalesced until the forwarding consumed the first.
            // A forwarding that never cancels lets this child exit normally,
            // which the parent reports as the failure.
            let forwarded = tokio::time::timeout(CHILD_BOUND, async {
                while !cancellation.is_cancelled() {
                    tokio::time::sleep(FORWARD_POLL).await;
                }
            })
            .await;
            if forwarded.is_err() {
                return;
            }
            signal_self("-TERM");
        }
        tokio::time::sleep(CHILD_BOUND).await;
    }

    /// Run this test binary as the interrupted child of `scenario`.
    fn run_child(test: &str, scenario: &str) -> std::process::ExitStatus {
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("test executable is unknown: {error}"));
        Command::new(executable)
            .args([test, "--exact", "--test-threads=1", "--nocapture"])
            .env(SCENARIO_ENV, scenario)
            .status()
            .unwrap_or_else(|error| panic!("interrupted child did not start: {error}"))
    }

    /// The child's own body when re-executed, or `None` in the parent.
    fn child_scenario() -> Option<String> {
        std::env::var(SCENARIO_ENV).ok()
    }

    #[tokio::test]
    async fn an_interrupt_after_the_request_ends_the_process_by_that_signal() {
        if let Some(scenario) = child_scenario() {
            return interrupted_child(&scenario).await;
        }
        let status = run_child(
            "supervised_index::interrupt_exit_tests::an_interrupt_after_the_request_ends_the_process_by_that_signal",
            AFTER_REQUEST,
        );
        // Like the default disposition: a shell sees death by SIGINT and
        // stops a loop or script, which a plain exit status would not do.
        assert_eq!(
            status.signal(),
            Some(signal_hook::consts::SIGINT),
            "{status:?}"
        );
    }

    #[tokio::test]
    async fn a_second_interrupt_ends_the_process_by_that_signal() {
        if let Some(scenario) = child_scenario() {
            return interrupted_child(&scenario).await;
        }
        let status = run_child(
            "supervised_index::interrupt_exit_tests::a_second_interrupt_ends_the_process_by_that_signal",
            SECOND_INTERRUPT,
        );
        assert_eq!(
            status.signal(),
            Some(signal_hook::consts::SIGTERM),
            "{status:?}"
        );
    }
}
