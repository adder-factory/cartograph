use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use cartograph_db::{CartographDatabase, LeaseError, ProjectLease};
use tokio::{
    sync::oneshot,
    task::{JoinError, JoinHandle},
    time::{Instant, MissedTickBehavior, interval_at, timeout_at},
};

use super::{OperationBudget, SupervisorError, ambiguous};
use crate::{CancellationReason, SupervisorConfig, progress::SharedProgress};

const HEARTBEAT_DATABASE_ATTEMPTS: u8 = 3;
const MIN_HEARTBEAT_RETRY_BUDGET: Duration = Duration::from_secs(1);
/// Stable operation identifier for lease renewal by a keeper.
pub(super) const HEARTBEAT_OPERATION: &str = "heartbeat";

/// When a lease keeper renews.
#[derive(Clone, Copy)]
pub(super) struct RenewalWindow {
    /// Instant of the first heartbeat; later ones follow every interval.
    first: Instant,
    /// No heartbeat starts at or after this instant, or is bounded past it.
    ceiling: Instant,
}

impl RenewalWindow {
    /// Renewal while pipeline work is active: the first heartbeat one interval
    /// from now and none at or after the work deadline.
    pub(super) fn active_work(config: SupervisorConfig, budget: OperationBudget) -> Self {
        Self {
            first: Instant::now() + config.deadlines.heartbeat_interval,
            ceiling: budget.work_deadline,
        }
    }

    /// Renewal while an aborted root task finishes a synchronous section: a
    /// heartbeat right away, because renewal paused for the cancellation grace,
    /// and none at or after `ceiling`, the end of the root work's bounded reap.
    pub(super) fn reaping(ceiling: Instant) -> Self {
        Self {
            first: Instant::now(),
            ceiling,
        }
    }
}

/// Inputs every keeper heartbeat needs.
pub(super) struct LeaseRenewal {
    pub(super) database: CartographDatabase,
    pub(super) config: SupervisorConfig,
    pub(super) budget: OperationBudget,
    pub(super) progress: SharedProgress,
    pub(super) window: RenewalWindow,
}

/// Result of one keeper heartbeat.
enum Renewal {
    Renewed(ProjectLease),
    Ended(KeeperExit),
}

impl LeaseRenewal {
    async fn renew(&self, lease: ProjectLease) -> Renewal {
        let attempt = match run_bounded_heartbeat(HeartbeatRequest {
            database: self.database.clone(),
            config: self.config,
            budget: self.budget,
            ceiling: self.window.ceiling,
            operation: HEARTBEAT_OPERATION,
            lease,
        })
        .await
        {
            Ok(attempt) => attempt,
            Err(error) => return Renewal::Ended(KeeperExit::failed(error)),
        };
        let reason = match attempt.result {
            Ok(()) => {
                self.progress.mark_heartbeat();
                return Renewal::Renewed(attempt.lease);
            }
            Err(LeaseError::Lost) => CancellationReason::LeaseLost,
            Err(_) => CancellationReason::LeaseHeartbeatFailed,
        };
        Renewal::Ended(KeeperExit::ended(
            Some(attempt.lease),
            LeaseVerdict::Cancel(reason),
        ))
    }
}

/// Lease renewal driven on its own Tokio task.
///
/// Renewal is independent of how the pipeline work and the monitor are
/// scheduled: the keeper is woken only by its interval timer, its own bounded
/// heartbeat request, and the stop signal, and it shares no lock with the work.
/// A work future that blocks or spins its thread therefore cannot delay the
/// lease heartbeat. The keeper owns the exact lease token while it runs and
/// hands it back when stopped; dropping this value aborts the keeper.
pub(super) struct LeaseKeeper {
    stop: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<KeeperExit>>,
    gate: Arc<RenewalGate>,
    /// Renewal activity when the monitor last began waiting for an event.
    observed: RenewalEpoch,
}

/// Stop signal and renewal gate shared between a keeper and its owner.
struct KeeperControl {
    stopped: oneshot::Receiver<()>,
    gate: Arc<RenewalGate>,
}

/// Arbitrates between a keeper starting a heartbeat and the monitor accepting
/// an event.
///
/// One atomic word holds the in-flight and frozen flags beside a count of
/// started heartbeats. The monitor's freeze stops any later heartbeat from
/// starting, so the acceptance point of an event is well defined, and returns
/// the renewal epoch at that point. Comparing it with the epoch the monitor
/// recorded before it selected the event reveals any heartbeat that overlapped
/// the selection, even one that finished between the selection and the freeze.
struct RenewalGate(AtomicU64);

/// Renewal activity at one instant: heartbeats started so far and whether one
/// is in flight. The frozen flag is not part of an epoch.
#[derive(Clone, Copy)]
struct RenewalEpoch(u64);

impl RenewalGate {
    /// A heartbeat is in flight.
    const RENEWING: u64 = 1;
    /// The monitor accepted an event; no heartbeat may start.
    const FROZEN: u64 = 1 << 1;
    /// One started heartbeat in the count kept above the two flags.
    const STARTED: u64 = 1 << 2;

    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Start one heartbeat unless renewal is frozen; true when it may start.
    ///
    /// A start that would overflow the count is refused like a frozen gate,
    /// which no keeper reaches at any supported heartbeat interval.
    fn begin(&self) -> bool {
        self.0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if state & (Self::RENEWING | Self::FROZEN) == 0 {
                    state
                        .checked_add(Self::STARTED)
                        .map(|counted| counted | Self::RENEWING)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// Finish one heartbeat; a freeze that arrived meanwhile stays in force.
    fn end(&self) {
        self.0.fetch_and(!Self::RENEWING, Ordering::AcqRel);
    }

    /// Renewal activity now.
    fn epoch(&self) -> RenewalEpoch {
        RenewalEpoch(self.0.load(Ordering::Acquire) & !Self::FROZEN)
    }

    /// Freeze renewal and return the activity at that moment.
    fn freeze(&self) -> RenewalEpoch {
        RenewalEpoch(self.0.fetch_or(Self::FROZEN, Ordering::AcqRel) & !Self::FROZEN)
    }
}

impl RenewalEpoch {
    /// Whether a heartbeat was in flight at any moment from `earlier` until
    /// this epoch: one is still in flight, or one started or finished since.
    const fn overlaps_since(self, earlier: Self) -> bool {
        self.0 & RenewalGate::RENEWING != 0 || self.0 != earlier.0
    }
}

/// Final state of a lease keeper.
pub(super) struct KeeperExit {
    /// The exact lease token, unless renewal could not vouch for it.
    pub(super) lease: Option<ProjectLease>,
    /// Why renewal ended on its own; `None` when it was stopped.
    pub(super) verdict: Option<LeaseVerdict>,
}

/// Why lease renewal ended before its owner stopped it.
pub(super) enum LeaseVerdict {
    /// PostgreSQL rejected the exact token or the heartbeat failed.
    Cancel(CancellationReason),
    /// The bounded heartbeat could not be completed, reconciled, or reaped.
    Failed(SupervisorError),
}

impl KeeperExit {
    const fn ended(lease: Option<ProjectLease>, verdict: LeaseVerdict) -> Self {
        Self {
            lease,
            verdict: Some(verdict),
        }
    }

    fn failed(error: SupervisorError) -> Self {
        Self::ended(None, LeaseVerdict::Failed(error))
    }

    fn joined(joined: Result<Self, JoinError>) -> Self {
        joined.unwrap_or_else(|_| Self::failed(ambiguous(HEARTBEAT_OPERATION)))
    }
}

impl LeaseKeeper {
    pub(super) fn spawn(renewal: LeaseRenewal, lease: ProjectLease) -> Self {
        let (stop, stopped) = oneshot::channel();
        let gate = Arc::new(RenewalGate::new());
        let observed = gate.epoch();
        let control = KeeperControl {
            stopped,
            gate: Arc::clone(&gate),
        };
        Self {
            stop: Some(stop),
            handle: Some(tokio::spawn(keep_lease(renewal, lease, control))),
            gate,
            observed,
        }
    }

    /// Record renewal activity as the monitor begins waiting for an event.
    pub(super) fn observe_renewal(&mut self) {
        self.observed = self.gate.epoch();
    }

    /// Accept an event: prevent further heartbeats and report whether one was
    /// in flight at any moment since the monitor last recorded renewal.
    ///
    /// A heartbeat in flight as the event was selected can finish before this
    /// freeze, so its in-flight flag alone cannot be trusted; the epoch still
    /// moved. Reporting heartbeats that started after the selection too is a
    /// deliberate over-approximation toward the monitor's former conservative
    /// priority.
    pub(super) fn freeze_renewal(&self) -> bool {
        self.gate.freeze().overlaps_since(self.observed)
    }

    /// Wait for renewal to end on its own with a verdict. Cancel-safe.
    pub(super) async fn exited(&mut self) -> KeeperExit {
        let Some(handle) = self.handle.as_mut() else {
            return std::future::pending().await;
        };
        let joined = handle.await;
        self.handle = None;
        KeeperExit::joined(joined)
    }

    /// Stop renewal after any in-flight bounded heartbeat and recover the lease.
    ///
    /// The keeper stays owned (and aborted on drop) while this waits. When the
    /// bound elapses the keeper is aborted without waiting further and its lease
    /// token is abandoned, which forbids owned cleanup.
    pub(super) async fn stop(&mut self, deadline: Instant) -> KeeperExit {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let joined = match self.handle.as_mut() {
            Some(handle) => timeout_at(deadline, handle).await,
            None => return KeeperExit::failed(ambiguous(HEARTBEAT_OPERATION)),
        };
        if let Ok(joined) = joined {
            self.handle = None;
            return KeeperExit::joined(joined);
        }
        // The aborted keeper stays owned, so dropping this value aborts it again.
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
        KeeperExit::failed(SupervisorError::UnreapedDurableOperation {
            operation: HEARTBEAT_OPERATION,
        })
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Renew the lease every heartbeat interval until stopped or renewal ends.
async fn keep_lease(
    renewal: LeaseRenewal,
    lease: ProjectLease,
    control: KeeperControl,
) -> KeeperExit {
    let KeeperControl { mut stopped, gate } = control;
    let interval = renewal.config.deadlines.heartbeat_interval;
    let mut heartbeat = interval_at(renewal.window.first, interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut lease = lease;
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => return KeeperExit { lease: Some(lease), verdict: None },
            _ = heartbeat.tick() => {},
        }
        if Instant::now() >= renewal.window.ceiling {
            // Past its ceiling the keeper only holds the token for its owner.
            // During active work the monitor's operation-deadline branch owns
            // the outcome, which its biased branch order always preferred to a
            // heartbeat; renewing there could only race that decision.
            let _ = (&mut stopped).await;
            return KeeperExit {
                lease: Some(lease),
                verdict: None,
            };
        }
        if !gate.begin() {
            // The monitor accepted an event and froze renewal; await its stop.
            let _ = (&mut stopped).await;
            return KeeperExit {
                lease: Some(lease),
                verdict: None,
            };
        }
        let renewed = renewal.renew(lease).await;
        gate.end();
        lease = match renewed {
            Renewal::Renewed(lease) => lease,
            Renewal::Ended(exit) => return exit,
        };
    }
}

/// Outcome of one bounded heartbeat request and the token it renewed.
pub(super) struct HeartbeatAttempt {
    pub(super) lease: ProjectLease,
    pub(super) result: Result<(), LeaseError>,
    /// The request outlived its own deadline and finished during its reap bound.
    pub(super) late: bool,
}

/// One bounded heartbeat of an exact lease token.
pub(super) struct HeartbeatRequest {
    pub(super) database: CartographDatabase,
    pub(super) config: SupervisorConfig,
    pub(super) budget: OperationBudget,
    pub(super) ceiling: Instant,
    pub(super) operation: &'static str,
    pub(super) lease: ProjectLease,
}

/// Renew an exact lease token, bounded by one request plus its reap horizon.
pub(super) async fn run_bounded_heartbeat(
    request: HeartbeatRequest,
) -> Result<HeartbeatAttempt, SupervisorError> {
    let HeartbeatRequest {
        database,
        config,
        budget,
        ceiling,
        operation,
        mut lease,
    } = request;
    let request_deadline = OperationBudget::database_deadline(config, ceiling)?;
    let mut task = AbortOnDrop::spawn(async move {
        let statement_timeout = config.deadlines.statement_timeout();
        let mut attempts_remaining =
            if config.deadlines.heartbeat_request >= MIN_HEARTBEAT_RETRY_BUDGET {
                HEARTBEAT_DATABASE_ATTEMPTS
            } else {
                1
            };
        // Each exact-token update is idempotent and server-capped at half one request. Three
        // attempts consume at most one and a half requests, leaving half of the already-bounded
        // request-plus-reap horizon for transaction setup and rollback. Sub-second custom
        // budgets use one attempt because their fixed transaction overhead cannot safely reserve
        // that margin. Lost ownership is never retried, and the outer deadlines still abort and
        // reap exhaustion.
        let result = loop {
            let result = database
                .heartbeat_lease_bounded(&mut lease, statement_timeout)
                .await;
            attempts_remaining = attempts_remaining.saturating_sub(1);
            if attempts_remaining == 0
                || !matches!(&result, Err(LeaseError::DatabaseOperation { .. }))
            {
                break result;
            }
        };
        (lease, result)
    });
    match timeout_at(request_deadline, &mut task).await {
        Ok(Ok((lease, result))) => Ok(HeartbeatAttempt {
            lease,
            result,
            late: false,
        }),
        Ok(Err(_)) => Err(ambiguous(operation)),
        Err(_) => {
            let reap_deadline = OperationBudget::request_deadline(
                config.deadlines.heartbeat_request,
                budget.final_deadline,
            );
            match timeout_at(reap_deadline, &mut task).await {
                Ok(Ok((lease, result))) => Ok(HeartbeatAttempt {
                    lease,
                    result,
                    late: true,
                }),
                Ok(Err(_)) => Err(ambiguous(operation)),
                Err(_) => {
                    task.abort();
                    let _ = (&mut task).await;
                    Err(SupervisorError::UnreapedDurableOperation { operation })
                }
            }
        }
    }
}

/// A spawned request task that is aborted when its owner is dropped.
///
/// A lease keeper aborted mid-heartbeat must not leave its database request
/// renewing the lease after the keeper and its token are gone.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T: Send + 'static> AbortOnDrop<T> {
    fn spawn<Task>(task: Task) -> Self
    where
        Task: Future<Output = T> + Send + 'static,
    {
        Self(tokio::spawn(task))
    }

    fn abort(&self) {
        self.0.abort();
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(context)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::RenewalGate;

    #[test]
    fn renewal_gate_reports_inflight_heartbeats_and_blocks_later_starts() {
        let idle = RenewalGate::new();
        let observed = idle.epoch();
        assert!(!idle.freeze().overlaps_since(observed));
        assert!(!idle.begin());

        let renewing = RenewalGate::new();
        let observed = renewing.epoch();
        assert!(renewing.begin());
        assert!(renewing.freeze().overlaps_since(observed));
        // A heartbeat that finishes after the freeze must not reopen renewal.
        renewing.end();
        assert!(!renewing.begin());

        // A heartbeat that finished before the monitor began waiting is not
        // part of the event it later accepts.
        let finished = RenewalGate::new();
        assert!(finished.begin());
        finished.end();
        let observed = finished.epoch();
        assert!(!finished.freeze().overlaps_since(observed));
    }

    #[test]
    fn freeze_reports_a_heartbeat_that_finished_after_the_event_was_selected() {
        // A heartbeat is in flight as the monitor begins waiting and as it
        // selects an event, then finishes before the monitor freezes renewal.
        // The gate is idle at the freeze, yet the heartbeat overlapped the
        // event, so the monitor must still re-apply its priority.
        let finished_late = RenewalGate::new();
        assert!(finished_late.begin());
        let observed = finished_late.epoch();
        finished_late.end();
        assert!(finished_late.freeze().overlaps_since(observed));

        // One that started and finished after the monitor began waiting is
        // reported too, the deliberate over-approximation.
        let started_later = RenewalGate::new();
        let observed = started_later.epoch();
        assert!(started_later.begin());
        started_later.end();
        assert!(started_later.freeze().overlaps_since(observed));
    }
}
