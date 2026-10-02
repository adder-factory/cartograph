//! Cooperative cancellation and parent supervision for `cartograph index`.
//!
//! Every index run turns its first SIGINT/SIGTERM (Ctrl-C on Windows) into a
//! cooperative [`ProjectCancellation`]: the indexer supervisor then fails its
//! staging generation and releases its exact project lease instead of leaving
//! both behind until the lease expires. A second interrupt exits immediately.
//! A cancelled request reports `request_cancelled` as its primary failure.
//! It adds the secondary `cleanup_failure` unless PostgreSQL confirms the
//! cleanup of what that request itself held. Other writers' leases and
//! generations on the same project are never part of that proof. MCP admin
//! index jobs run the same direct path under their job cancellation, so both
//! surfaces report a cancelled index's cleanup identically.
//!
//! The hidden `--supervised` mode is the child that `upgrade --apply` runs.
//! Its parent holds stdin open and closes it to request the same cooperative
//! cancellation, reads one compact JSON progress line on stderr whenever the
//! observable work changes, and relies on this module waiting (within
//! [`SUPERVISED_WRITER_WAIT`]) for another live writer on the project instead
//! of failing immediately with `lease_busy`, including a writer that started
//! while this child scanned the checkout and now blocks its attempt.
//! `sync-if-dirty` waits on the same project leases through
//! [`wait_for_project_writers`] after its own `lease_busy` attempts.

use std::{
    io::{Read, Write as _},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use cartograph_agent::{
    IndexFailure, IndexOptions, IndexReport, PipelineStage, ProjectCancellation, ProjectError,
    ProjectRuntime, ReservedIndexGeneration, SupervisorState, SupervisorStatus,
};
use cartograph_db::LeaseTarget;
use cartograph_domain::{GenerationId, GenerationState, ProjectId, ProjectOperation};
use serde::Serialize;
use tokio::time::{Instant, MissedTickBehavior};

/// Longest a supervised index waits for other live writers (for example an
/// MCP server's auto-sync) to release the project before it reports
/// `lease_busy`. Each writer's own supervisor bounds its run.
pub(crate) const SUPERVISED_WRITER_WAIT: Duration = Duration::from_mins(30);
/// Interval between reads of the project's operation leases.
const WRITER_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// First pause after a `lease_busy` attempt that no live lease explained,
/// such as a project lock held past the bounded staging-preflight wait by an
/// operation without a lease, or a schema-maintenance lock; it doubles up to
/// [`MAXIMUM_COLLISION_BACKOFF`] because every collision costs a source scan.
/// A collision with a live writer instead waits for that writer's lease.
const INITIAL_COLLISION_BACKOFF: Duration = Duration::from_secs(15);
/// Longest pause between acquisition collisions.
const MAXIMUM_COLLISION_BACKOFF: Duration = Duration::from_mins(4);
/// Interval between progress observations; a line is written only on change.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Scratch buffer for discarding unexpected bytes written to a supervised stdin.
const STDIN_DRAIN_BYTES: usize = 256;
/// Conventional exit status for a process stopped by a repeated interrupt.
const INTERRUPTED_EXIT_CODE: i32 = 130;
/// Every operation whose live lease makes index acquisition on the project
/// fail with `Busy`; the test-only `operation_slot` keeps it exhaustive.
const PROJECT_OPERATIONS: [ProjectOperation; 5] = [
    ProjectOperation::Index,
    ProjectOperation::Sync,
    ProjectOperation::Hook,
    ProjectOperation::Migration,
    ProjectOperation::Rebuild,
];

/// Position of `operation` in [`PROJECT_OPERATIONS`]. A new operation fails
/// to compile here until the writer wait observes it too.
#[cfg(test)]
const fn operation_slot(operation: ProjectOperation) -> usize {
    match operation {
        ProjectOperation::Index => 0,
        ProjectOperation::Sync => 1,
        ProjectOperation::Hook => 2,
        ProjectOperation::Migration => 3,
        ProjectOperation::Rebuild => 4,
    }
}

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
}

/// Run one index request under its cancellation signal.
///
/// A failure keeps the attempt's primary error and whether cleanup of what
/// the request held also failed; for a cancellation, PostgreSQL decides the
/// latter (see [`confirm_cancellation_cleanup`]).
pub(crate) async fn run_cancellable_index(
    request: CancellableIndex<'_>,
) -> Result<IndexReport, IndexFailure> {
    let CancellableIndex {
        runtime,
        options,
        cancellation,
        supervision,
    } = request;
    match supervision {
        IndexSupervision::Direct => {
            let mut owned = OwnedReservations::default();
            let reserved_before = cancellation.reserved_index_generation();
            let result = runtime
                .index_with_cancellation_detail(options, cancellation.clone())
                .await;
            owned.record_since(&cancellation, reserved_before.as_ref());
            confirm_cancellation_cleanup(FinishedRequest {
                result,
                runtime,
                cancelled: cancellation.is_cancelled(),
                owned: &owned,
            })
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
/// The listeners are installed synchronously when this is created, before any
/// work starts, and are never removed: tokio never restores a signal's
/// default disposition, so removing them would make every later interrupt
/// silently ignored, including during the remaining steps of a caller such as
/// `install`. While the request runs, the first interrupt cancels it
/// cooperatively and a second exits immediately; once
/// [`Self::request_finished`] is called, any interrupt exits immediately. A
/// process runs at most one index request, so one forwarding is installed.
pub(crate) struct InterruptForwarding {
    finished: Arc<AtomicBool>,
}

impl InterruptForwarding {
    /// Install the listeners and forward interrupts into `cancellation`;
    /// `announce` tells an interactive operator that the stop was accepted.
    /// When no listener can be installed, default signal behavior remains.
    /// The forwarding task runs until the runtime shuts down.
    pub(crate) fn install(cancellation: ProjectCancellation, announce: bool) -> Self {
        let finished = Arc::new(AtomicBool::new(false));
        if let Some(interrupts) = Interrupts::listen() {
            drop(tokio::spawn(forward_interrupts(
                interrupts,
                InterruptRoute {
                    cancellation,
                    announce,
                    finished: finished.clone(),
                },
            )));
        }
        Self { finished }
    }

    /// The index request ended; any later interrupt exits immediately, as
    /// the default disposition would have.
    pub(crate) fn request_finished(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }
}

/// Every generation one index request reserved, across all of its attempts.
///
/// A supervised request retries after a collision, and a collided attempt's
/// own staging cleanup can still be blocked behind the other writer, so a
/// later cancellation must account for every reservation, not only the last.
#[derive(Default)]
struct OwnedReservations {
    reserved: Vec<ReservedIndexGeneration>,
}

impl OwnedReservations {
    /// Record the reservation `cancellation` gained since `before`, if any.
    fn record_since(
        &mut self,
        cancellation: &ProjectCancellation,
        before: Option<&ReservedIndexGeneration>,
    ) {
        if let Some(reserved) = cancellation
            .reserved_index_generation()
            .filter(|reserved| Some(reserved) != before)
        {
            self.reserved.push(reserved);
        }
    }
}

/// One finished index request whose cancellation may need a cleanup proof.
struct FinishedRequest<'request> {
    result: Result<IndexReport, IndexFailure>,
    runtime: &'request ProjectRuntime,
    /// Whether the request's cancellation was requested.
    cancelled: bool,
    owned: &'request OwnedReservations,
}

/// Report a cancelled request as `request_cancelled`, with a secondary
/// cleanup failure unless PostgreSQL proves the request cleaned up what it
/// held.
///
/// Neither the agent's cleanup flag nor its absence settles a cancellation.
/// The agent reports any supervisor error under a cancelled token as
/// `RequestCancelled`, including one whose owned cleanup failed or was
/// skipped. Conversely, an attempt whose own cleanup already completed can
/// still carry a cleanup failure when a redundant bounded cleanup afterwards
/// waits behind another writer's project lock; and a supervised request may
/// hold reservations from earlier attempts. A request that reserved no
/// generation held nothing. Otherwise each generation it reserved must have
/// left `staging`/`ready` (cleanup marks it `failed`; it may also have been
/// published, superseded, or retired) and the project's index lease row must
/// name none of them. Another writer's lease and generations, which a
/// concurrent auto-sync may hold at the same moment, are not this request's
/// and never count against it. An unreadable row is reported as a cleanup
/// failure: conservative, never optimistic.
async fn confirm_cancellation_cleanup(
    request: FinishedRequest<'_>,
) -> Result<IndexReport, IndexFailure> {
    let FinishedRequest {
        result,
        runtime,
        cancelled,
        owned,
    } = request;
    match &result {
        Err(failure) if cancellation_outcome(failure.error(), cancelled) => {}
        _ => return result,
    }
    let cancellation = IndexFailure::from(ProjectError::RequestCancelled);
    if owned_cleanup_proven(runtime, owned).await {
        Err(cancellation)
    } else {
        Err(cancellation.with_failed_cleanup())
    }
}

/// Whether a failure is the outcome of a cancellation that the cleanup
/// proof settles: `RequestCancelled`, or `lease_busy` after cancellation was
/// requested.
///
/// A cancelled attempt that reserved a generation already reports
/// `RequestCancelled`. One that met another writer before reserving (a live
/// lease, or the project lock still held past the bounded staging-preflight
/// wait) answers `lease_busy` without checking cancellation, yet it is the
/// requested stop, not contention left to retry, so it is reported as one;
/// so is a collision that ended just before the stop arrived, which the same
/// proof settles. Any other primary failure, including a pre-reservation
/// staging recovery that failed for a reason other than contention, is not
/// the request's own cleanup and is reported as it is.
const fn cancellation_outcome(error: &ProjectError, cancelled: bool) -> bool {
    match error {
        ProjectError::RequestCancelled => true,
        ProjectError::IndexLeaseBusy => cancelled,
        _ => false,
    }
}

/// Whether PostgreSQL shows every generation in `owned` settled and no index
/// lease row naming one of them; trivially true when nothing was reserved.
async fn owned_cleanup_proven(runtime: &ProjectRuntime, owned: &OwnedReservations) -> bool {
    let Some(project_id) = owned.reserved.first().map(|first| first.project_id.clone()) else {
        return true;
    };
    let database = runtime.database();
    let mut generations = Vec::with_capacity(owned.reserved.len());
    let mut generation_ids = Vec::with_capacity(owned.reserved.len());
    for reserved in &owned.reserved {
        generations.push(
            database
                .generation_state(&reserved.project_id, &reserved.generation_id)
                .await
                .map_or(OwnedGeneration::Unreadable, |state| {
                    state.map_or(OwnedGeneration::Settled, OwnedGeneration::from_state)
                }),
        );
        generation_ids.push(reserved.generation_id.clone());
    }
    let target = LeaseTarget::new(project_id, ProjectOperation::Index, None);
    let lease = match database.lease_status(&target).await {
        Ok(None) => IndexLeaseRow::Absent,
        Ok(Some(lease)) => IndexLeaseRow::Names(lease.generation_id().cloned()),
        Err(_) => IndexLeaseRow::Unreadable,
    };
    owned_cleanup_confirmed(&OwnedCleanup {
        owned: &generation_ids,
        generations: &generations,
        lease,
    })
}

/// One generation the cancelled request reserved, as PostgreSQL reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedGeneration {
    /// Failed, published, superseded, retiring, or already removed.
    Settled,
    /// Still `staging` or `ready`: cleanup did not fail it.
    Unpublished,
    /// The state could not be read.
    Unreadable,
}

impl OwnedGeneration {
    const fn from_state(state: GenerationState) -> Self {
        match state {
            GenerationState::Staging | GenerationState::Ready => Self::Unpublished,
            GenerationState::Current
            | GenerationState::Superseded
            | GenerationState::Failed
            | GenerationState::Retiring => Self::Settled,
        }
    }
}

/// The project's index lease row at proof time.
#[derive(Clone, Debug, PartialEq, Eq)]
enum IndexLeaseRow {
    /// No index lease row exists.
    Absent,
    /// A row exists and names this generation, if any.
    Names(Option<GenerationId>),
    /// The row could not be read.
    Unreadable,
}

/// Evidence about one cancelled request's own cleanup.
struct OwnedCleanup<'evidence> {
    /// Every generation the request reserved.
    owned: &'evidence [GenerationId],
    /// Their states, in the same order.
    generations: &'evidence [OwnedGeneration],
    lease: IndexLeaseRow,
}

/// Whether every generation the request reserved settled and none of them is
/// still named by the project's index lease. A lease row naming any other
/// generation, or none, belongs to another writer and does not count.
fn owned_cleanup_confirmed(evidence: &OwnedCleanup<'_>) -> bool {
    let lease_released = match &evidence.lease {
        IndexLeaseRow::Absent => true,
        IndexLeaseRow::Names(named) => named
            .as_ref()
            .is_none_or(|named| !evidence.owned.contains(named)),
        IndexLeaseRow::Unreadable => false,
    };
    lease_released
        && evidence
            .generations
            .iter()
            .all(|generation| *generation == OwnedGeneration::Settled)
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
    finished: Arc<AtomicBool>,
}

async fn forward_interrupts(mut interrupts: Interrupts, route: InterruptRoute) {
    if !interrupts.next().await {
        return;
    }
    if route.finished.load(Ordering::Relaxed) {
        std::process::exit(INTERRUPTED_EXIT_CODE);
    }
    route.cancellation.cancel();
    if route.announce {
        eprintln!(
            "cartograph: stopping the index cooperatively so it releases its project lease; interrupt again to exit immediately"
        );
    }
    if interrupts.next().await {
        std::process::exit(INTERRUPTED_EXIT_CODE);
    }
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

    /// Wait for the next interrupt; false once a signal stream closed.
    async fn next(&mut self) -> bool {
        tokio::select! {
            received = next_signal(self.interrupt.as_mut()) => received,
            received = next_signal(self.terminate.as_mut()) => received,
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

    /// Wait for the next interrupt; false once the signal stream closed.
    async fn next(&mut self) -> bool {
        self.ctrl_c.recv().await.is_some()
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

    async fn next(&mut self) -> bool {
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

/// Wait out other live writers, then index. A collision with another writer
/// is awaited again within the same bound: one whose live lease explains it
/// waits for that lease, and an unexplained one pauses for a growing interval.
/// A cancellation at any point, including while waiting, is confirmed against
/// every generation the request's attempts reserved.
async fn index_after_other_writers(
    wait: &WriterWait<'_>,
    options: IndexOptions,
) -> Result<IndexReport, IndexFailure> {
    let mut owned = OwnedReservations::default();
    let result = attempt_until_final(wait, options, &mut owned).await;
    confirm_cancellation_cleanup(FinishedRequest {
        result,
        runtime: wait.runtime,
        cancelled: wait.cancellation.is_cancelled(),
        owned: &owned,
    })
    .await
}

/// Run attempts until one is final, recording each attempt's reservation.
/// The wait itself ends the request with `request_cancelled` or, once its
/// bound expires, `lease_busy`; neither ran a cleanup of its own.
async fn attempt_until_final(
    wait: &WriterWait<'_>,
    options: IndexOptions,
    owned: &mut OwnedReservations,
) -> Result<IndexReport, IndexFailure> {
    let mut backoff = INITIAL_COLLISION_BACKOFF;
    loop {
        wait.progress.indexing.store(false, Ordering::Relaxed);
        wait_for_other_writers(wait).await?;
        wait.progress.attempts.fetch_add(1, Ordering::Relaxed);
        wait.progress.indexing.store(true, Ordering::Relaxed);
        let reserved_before = wait.cancellation.reserved_index_generation();
        let result = wait
            .runtime
            .index_with_cancellation_detail(options.clone(), wait.cancellation.clone())
            .await;
        owned.record_since(wait.cancellation, reserved_before.as_ref());
        match attempt_verdict(wait, &result).await {
            AttemptVerdict::Final => return result,
            AttemptVerdict::AwaitWriter => {}
            AttemptVerdict::Backoff => {
                wait.progress.indexing.store(false, Ordering::Relaxed);
                within_wait(wait, tokio::time::sleep(backoff)).await?;
                backoff = backoff.saturating_mul(2).min(MAXIMUM_COLLISION_BACKOFF);
            }
        }
    }
}

/// What a finished attempt means for the supervised loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptVerdict {
    /// Report the attempt's own result.
    Final,
    /// Another writer's live lease explains the failure: wait it out.
    AwaitWriter,
    /// The project was busy without a live lease to explain it, for example
    /// a schema-maintenance lock: pause, then try again.
    Backoff,
}

/// Whether another project writer refused the attempt.
///
/// The agent reports every collision as `lease_busy`: a live project lease, a
/// project lock still held past the bounded staging-preflight wait (another
/// writer's prepare transaction holds it for its whole COPY), or a lock that
/// kept the lease read waiting past that wait, before any generation is
/// reserved, and a writer that won after that check at lease acquisition. A cleanup failure that follows the collision is secondary
/// detail of that primary. `index_cleanup_failed` as the primary failure
/// means only that the pre-reservation staging recovery failed for a reason
/// other than contention, so it is a final fault, never a collision.
const fn refused_by_another_writer(result: &Result<IndexReport, IndexFailure>) -> bool {
    match result {
        Err(failure) => matches!(failure.error(), ProjectError::IndexLeaseBusy),
        Ok(_) => false,
    }
}

/// Decide a collision from whether another writer's lease is live.
const fn contention_verdict(live_writer: bool) -> AttemptVerdict {
    if live_writer {
        AttemptVerdict::AwaitWriter
    } else {
        AttemptVerdict::Backoff
    }
}

/// Classify one finished attempt. A cancelled request reports its own
/// result, which the cleanup proof then settles. The contention probe runs
/// inside the wait's bounds: cancellation during it does the same, and an
/// expired wait during it leaves the writer wait that follows to report the
/// bound as `lease_busy`.
async fn attempt_verdict(
    wait: &WriterWait<'_>,
    result: &Result<IndexReport, IndexFailure>,
) -> AttemptVerdict {
    if wait.cancellation.is_cancelled() || !refused_by_another_writer(result) {
        return AttemptVerdict::Final;
    }
    match within_wait(wait, live_writer_on_project(wait.runtime)).await {
        Ok(live_writer) => contention_verdict(live_writer),
        Err(ProjectError::RequestCancelled) => AttemptVerdict::Final,
        Err(_) => AttemptVerdict::AwaitWriter,
    }
}

/// Whether any operation holds a live lease on the project right now.
async fn live_writer_on_project(runtime: &ProjectRuntime) -> bool {
    match registered_project(runtime).await {
        Some(project_id) => live_writer_heartbeat(runtime, &project_id).await.is_some(),
        None => false,
    }
}

struct WriterWait<'wait> {
    runtime: &'wait ProjectRuntime,
    progress: &'wait SupervisedProgress,
    cancellation: &'wait ProjectCancellation,
    deadline: Instant,
}

/// Wait out other writers on the request's registered project; see
/// [`wait_for_project_leases`].
async fn wait_for_other_writers(wait: &WriterWait<'_>) -> Result<(), ProjectError> {
    let Some(project_id) = within_wait(wait, registered_project(wait.runtime)).await? else {
        return Ok(());
    };
    wait_for_project_leases(wait, &project_id).await
}

/// Wait, until `deadline`, for every live operation lease on the project to
/// end; a deadline that passes first reports the retryable `lease_busy`.
///
/// `sync-if-dirty` waits here after an attempt reports `lease_busy`. A live
/// lease of any operation in [`PROJECT_OPERATIONS`] refuses index acquisition,
/// so waiting on the index lease alone would retry, and rescan the checkout,
/// while a sync, hook, migration, or rebuild lease is still live. The wait is
/// the supervised one without cancellation or progress reporting.
pub(crate) async fn wait_for_project_writers(
    runtime: &ProjectRuntime,
    project_id: &ProjectId,
    deadline: Instant,
) -> Result<(), ProjectError> {
    let progress = SupervisedProgress::default();
    let cancellation = ProjectCancellation::new();
    let wait = WriterWait {
        runtime,
        progress: &progress,
        cancellation: &cancellation,
        deadline,
    };
    wait_for_project_leases(&wait, project_id).await
}

/// Poll the project's operation leases while another writer holds one.
///
/// Missing, expired, or unreadable lease rows end the wait: they never
/// authorize anything, because the index's own exact lease acquisition stays
/// the only authority. Each renewal observed from another writer advances
/// `writer_heartbeats`, so the supervising parent sees the wait is live.
async fn wait_for_project_leases(
    wait: &WriterWait<'_>,
    project_id: &ProjectId,
) -> Result<(), ProjectError> {
    let mut last_heartbeat = None;
    loop {
        let Some(heartbeat) =
            within_wait(wait, live_writer_heartbeat(wait.runtime, project_id)).await?
        else {
            return Ok(());
        };
        if last_heartbeat.as_ref() != Some(&heartbeat) {
            wait.progress
                .writer_heartbeats
                .fetch_add(1, Ordering::Relaxed);
            last_heartbeat = Some(heartbeat);
        }
        within_wait(wait, tokio::time::sleep(WRITER_POLL_INTERVAL)).await?;
    }
}

/// The project's identifier, read without creating or touching its row.
/// `None` when the root was never registered, so no lease can exist on it,
/// or when the row is unreadable, which never authorizes anything either.
async fn registered_project(runtime: &ProjectRuntime) -> Option<ProjectId> {
    runtime
        .database()
        .project_snapshot_by_root(runtime.root_identity())
        .await
        .ok()
        .flatten()
        .map(|snapshot| snapshot.project_id)
}

/// The renewal timestamps of every live operation lease on the project, or
/// `None` when no live lease is readable.
async fn live_writer_heartbeat(
    runtime: &ProjectRuntime,
    project_id: &ProjectId,
) -> Option<Vec<String>> {
    let mut heartbeats = Vec::new();
    for operation in PROJECT_OPERATIONS {
        let target = LeaseTarget::new(project_id.clone(), operation, None);
        if let Ok(Some(lease)) = runtime.database().lease_status(&target).await
            && !lease.expired()
        {
            heartbeats.push(lease.heartbeat_at().to_owned());
        }
    }
    (!heartbeats.is_empty()).then_some(heartbeats)
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

#[derive(Serialize)]
struct ProgressLine<'snapshot> {
    progress: &'snapshot ProgressSnapshot,
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
fn emit_progress(snapshot: &ProgressSnapshot) {
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

    fn failed(failure: IndexFailure) -> Result<IndexReport, IndexFailure> {
        Err(failure)
    }

    #[test]
    fn only_lease_contention_is_a_collision_even_with_a_failed_cleanup_beside_it() {
        // A collided attempt whose own staging cleanup then timed out behind
        // the other writer keeps `lease_busy` as its primary: still a
        // collision to wait out, not a final cleanup failure.
        let collided = IndexFailure::from(ProjectError::IndexLeaseBusy).with_failed_cleanup();
        assert!(refused_by_another_writer(&failed(collided)));
        assert!(refused_by_another_writer(&failed(
            ProjectError::IndexLeaseBusy.into()
        )));
        assert_eq!(contention_verdict(true), AttemptVerdict::AwaitWriter);
        assert_eq!(contention_verdict(false), AttemptVerdict::Backoff);

        // A lock wait behind another writer's prepare is `lease_busy` before
        // reservation, so `index_cleanup_failed` as the primary is a real
        // staging-recovery fault: final, whether or not a writer is live.
        for other in [
            ProjectError::IndexCleanupFailed,
            ProjectError::RequestCancelled,
            ProjectError::IndexFailed,
            ProjectError::IndexLeaseFailed,
        ] {
            assert!(!refused_by_another_writer(&failed(other.into())));
        }
    }

    fn generation(value: &str) -> GenerationId {
        GenerationId::parse(value)
            .unwrap_or_else(|error| panic!("fixture generation id is invalid: {error}"))
    }

    #[test]
    fn a_cancelled_requests_contention_is_settled_by_the_proof_but_other_faults_are_not() {
        assert!(cancellation_outcome(&ProjectError::RequestCancelled, false));
        assert!(cancellation_outcome(&ProjectError::RequestCancelled, true));
        // A stop that met another writer before reserving anything is the
        // requested cancellation, not contention left to retry.
        assert!(cancellation_outcome(&ProjectError::IndexLeaseBusy, true));
        assert!(!cancellation_outcome(&ProjectError::IndexLeaseBusy, false));
        // A failed pre-reservation staging recovery is not this request's own
        // cleanup, so no proof about its reservations may relabel it.
        assert!(!cancellation_outcome(
            &ProjectError::IndexCleanupFailed,
            true
        ));
        assert!(!cancellation_outcome(&ProjectError::IndexLeaseFailed, true));
    }

    #[test]
    fn a_cancellation_proof_covers_only_what_the_attempt_held() {
        let owned = generation("00000000-0000-4000-8000-000000000001");
        let foreign = generation("00000000-0000-4000-8000-000000000002");
        let confirmed = |state, lease| {
            owned_cleanup_confirmed(&OwnedCleanup {
                owned: std::slice::from_ref(&owned),
                generations: &[state],
                lease,
            })
        };
        // Another writer's lease on the project, naming its own generation or
        // none, does not count against this request's cleanup.
        for lease in [
            IndexLeaseRow::Absent,
            IndexLeaseRow::Names(Some(foreign.clone())),
            IndexLeaseRow::Names(None),
        ] {
            assert!(confirmed(OwnedGeneration::Settled, lease));
        }
        // The request's own lease row, its own unfailed generation, or an
        // unreadable row is not a confirmed cleanup.
        assert!(!confirmed(
            OwnedGeneration::Settled,
            IndexLeaseRow::Names(Some(owned.clone()))
        ));
        assert!(!confirmed(
            OwnedGeneration::Unpublished,
            IndexLeaseRow::Absent
        ));
        assert!(!confirmed(
            OwnedGeneration::Unreadable,
            IndexLeaseRow::Absent
        ));
        assert!(!confirmed(
            OwnedGeneration::Settled,
            IndexLeaseRow::Unreadable
        ));
        // An earlier collided attempt's reservation counts too: it may still
        // be staging when a later wait is cancelled.
        let retried = generation("00000000-0000-4000-8000-000000000003");
        let both = [retried.clone(), owned.clone()];
        let across_attempts = |generations: &[OwnedGeneration], lease| {
            owned_cleanup_confirmed(&OwnedCleanup {
                owned: &both,
                generations,
                lease,
            })
        };
        assert!(!across_attempts(
            &[OwnedGeneration::Unpublished, OwnedGeneration::Settled],
            IndexLeaseRow::Absent
        ));
        assert!(!across_attempts(
            &[OwnedGeneration::Settled, OwnedGeneration::Settled],
            IndexLeaseRow::Names(Some(retried))
        ));
        assert!(across_attempts(
            &[OwnedGeneration::Settled, OwnedGeneration::Settled],
            IndexLeaseRow::Names(Some(foreign))
        ));
        for state in [GenerationState::Staging, GenerationState::Ready] {
            assert_eq!(
                OwnedGeneration::from_state(state),
                OwnedGeneration::Unpublished
            );
        }
        for state in [
            GenerationState::Current,
            GenerationState::Superseded,
            GenerationState::Failed,
            GenerationState::Retiring,
        ] {
            assert_eq!(OwnedGeneration::from_state(state), OwnedGeneration::Settled);
        }
    }

    #[test]
    fn writer_wait_observes_every_operation_that_blocks_index_acquisition() {
        for (slot, operation) in PROJECT_OPERATIONS.into_iter().enumerate() {
            assert_eq!(operation_slot(operation), slot);
        }
    }

    /// Bound of each live wait that another operation's lease outlasts, and
    /// how long the competing writer holds its lease before releasing it.
    const LIVE_WAIT_BOUND: Duration = Duration::from_millis(1_500);
    /// Bound of the wait that the competing writer's release ends first.
    const RELEASED_WAIT_BOUND: Duration = Duration::from_secs(30);
    /// The competing lease outlives every wait in the test.
    const COMPETING_LEASE: Duration = Duration::from_mins(1);

    /// Take another writer's live `operation` lease on the project.
    async fn competing_lease(
        runtime: &ProjectRuntime,
        project_id: &ProjectId,
        operation: ProjectOperation,
    ) -> cartograph_db::ProjectLease {
        runtime
            .database()
            .acquire_lease(cartograph_db::LeaseRequest::new(
                LeaseTarget::new(project_id.clone(), operation, None),
                cartograph_db::LeaseOwner::new(std::process::id(), "competing-project-writer"),
                COMPETING_LEASE,
            ))
            .await
            .unwrap_or_else(|error| panic!("competing {operation:?} lease failed: {error}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires PostgreSQL 18 with pg_search and pgvector"]
    async fn project_writer_wait_outlasts_every_operations_live_lease() {
        let url = std::env::var("CARTOGRAPH_TEST_DATABASE_URL")
            .unwrap_or_else(|_| panic!("live writer-wait database is not configured"));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let schema = format!("cg_cli_writer_wait_{}_{nanos}", std::process::id());
        let _schema_guard = cartograph_test_support::TestSchemaGuard::new(&url, schema.clone())
            .unwrap_or_else(|error| panic!("writer-wait schema guard failed: {error}"));
        let settings = cartograph_config::DatabaseSettings::parse(&url, Some("4"), Some("10000"))
            .and_then(|settings| settings.with_schema(&schema))
            .unwrap_or_else(|error| panic!("writer-wait settings failed: {error}"));
        let project = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        std::fs::write(project.path().join("lib.rs"), "pub fn waited() {}\n")
            .unwrap_or_else(|error| panic!("writer-wait fixture failed: {error}"));
        let runtime = ProjectRuntime::connect(project.path(), &settings)
            .await
            .unwrap_or_else(|error| panic!("writer-wait runtime failed: {error}"));
        let project_id = runtime
            .index(IndexOptions::default().with_history_refresh(false))
            .await
            .unwrap_or_else(|error| panic!("writer-wait initial index failed: {error}"))
            .project_id;

        // Every operation's live lease refuses index acquisition, so a wait
        // that ignored one would retry (and rescan the checkout) at once. A
        // wait that outlives its bound is the retryable contention code.
        for operation in PROJECT_OPERATIONS {
            let lease = competing_lease(&runtime, &project_id, operation).await;
            let waited =
                wait_for_project_writers(&runtime, &project_id, Instant::now() + LIVE_WAIT_BOUND)
                    .await;
            assert_eq!(
                waited,
                Err(ProjectError::IndexLeaseBusy),
                "the wait did not hold for a live {operation:?} lease"
            );
            runtime
                .database()
                .release_lease(&lease)
                .await
                .unwrap_or_else(|error| panic!("{operation:?} lease release failed: {error}"));
        }

        // A writer that releases its lease mid-wait ends the wait.
        let lease = competing_lease(&runtime, &project_id, ProjectOperation::Sync).await;
        let started = Instant::now();
        let release = async {
            tokio::time::sleep(LIVE_WAIT_BOUND).await;
            runtime.database().release_lease(&lease).await
        };
        let (waited, released) = tokio::join!(
            wait_for_project_writers(&runtime, &project_id, started + RELEASED_WAIT_BOUND),
            release
        );
        released.unwrap_or_else(|error| panic!("sync lease release failed: {error}"));
        assert_eq!(waited, Ok(()));
        assert!(started.elapsed() >= LIVE_WAIT_BOUND);
        runtime.close().await;
    }

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
