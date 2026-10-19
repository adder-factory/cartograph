use std::{
    collections::BTreeMap,
    future::{Future, poll_fn},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use thiserror::Error;
use tokio::{
    sync::oneshot,
    task::{AbortHandle, Id, JoinSet},
    time::Instant,
};

use crate::{PipelineFailure, PipelineStage};

/// Result handle for a worker registered with the supervisor-owned task scope.
pub struct ScopedTask<T> {
    receiver: oneshot::Receiver<Result<T, ScopedTaskError>>,
    observed: Arc<AtomicBool>,
    abort: AbortHandle,
}

pub(crate) struct TaskReservationGuard {
    observed: Arc<AtomicBool>,
}

impl TaskReservationGuard {
    pub(crate) fn acknowledge(self) {
        self.observed.store(true, Ordering::Release);
    }

    pub(crate) fn cancel(self) {
        self.acknowledge();
    }
}

impl<T> ScopedTask<T> {
    /// Wait for the worker result without detaching it from supervisor ownership.
    /// # Errors
    ///
    /// Returns an error if the scoped worker reports a pipeline failure or is
    /// cancelled before delivering its owned result.
    pub async fn join(self) -> Result<T, ScopedTaskError> {
        match self.receiver.await {
            Ok(result) => {
                self.observed.store(true, Ordering::Release);
                result
            }
            Err(_) => Err(ScopedTaskError::Cancelled),
        }
    }

    pub(crate) async fn join_retaining_reservation(
        self,
    ) -> Result<(T, TaskReservationGuard), ScopedTaskError> {
        match self.receiver.await {
            Ok(Ok(output)) => Ok((
                output,
                TaskReservationGuard {
                    observed: self.observed,
                },
            )),
            Ok(Err(error)) => {
                self.observed.store(true, Ordering::Release);
                Err(error)
            }
            Err(_) => Err(ScopedTaskError::Cancelled),
        }
    }

    pub(crate) fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }
}

/// Stable structured-concurrency failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ScopedTaskError {
    /// The supervisor closed the scope before this worker could be registered.
    #[error("Cartograph indexer worker scope is closed")]
    ScopeClosed,
    /// The configured task admission cap is exhausted; there is no hidden queue.
    #[error("Cartograph indexer worker task capacity is exhausted")]
    TaskCapacityExceeded,
    /// The configured in-flight byte budget cannot admit this worker.
    #[error("Cartograph indexer worker byte capacity is exhausted")]
    ByteCapacityExceeded,
    /// Every worker must reserve a nonzero bounded number of bytes.
    #[error("Cartograph indexer worker byte reservation is invalid")]
    InvalidByteReservation,
    /// The worker was aborted because its supervised operation ended.
    #[error("Cartograph indexer worker was cancelled")]
    Cancelled,
    /// The worker returned a supervisor-understood stage failure.
    #[error("Cartograph indexer worker failed during the {stage:?} stage")]
    WorkerFailed {
        /// Stable failed pipeline stage.
        stage: PipelineStage,
    },
    /// The worker panicked or its Tokio task ended unexpectedly.
    #[error("Cartograph indexer worker ended unexpectedly")]
    WorkerPanicked,
    /// The task registry could not be safely accessed.
    #[error("Cartograph indexer worker registry is unavailable")]
    RegistryUnavailable,
}

#[derive(Clone)]
pub(crate) struct TaskScope {
    registry: Arc<Mutex<TaskRegistry>>,
}

struct TaskRegistry {
    closed: bool,
    max_tasks: usize,
    max_bytes: u64,
    reserved_bytes: u64,
    workers: JoinSet<WorkerCompletion>,
    worker_controls: BTreeMap<Id, AbortHandle>,
    pending_observations: Vec<PendingObservation>,
    worker_failed: bool,
    unobserved_results: bool,
    #[cfg(test)]
    completion_gate: Option<oneshot::Receiver<()>>,
}

struct PendingObservation {
    observed: Arc<AtomicBool>,
    reserved_bytes: u64,
}

struct WorkerCompletion {
    worker_id: Id,
    outcome: WorkerOutcome,
    delivered: bool,
    observed: Arc<AtomicBool>,
    reserved_bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct ReapReport {
    pub(crate) active_tasks: usize,
    pub(crate) all_joined: bool,
    pub(crate) worker_failed: bool,
    pub(crate) unobserved_results: bool,
}

impl TaskScope {
    pub(crate) fn close_and_abort(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.closed = true;
            for control in registry.worker_controls.values() {
                control.abort();
            }
        }
    }

    pub(crate) fn new(max_tasks: usize, max_bytes: u64) -> Self {
        Self {
            registry: Arc::new(Mutex::new(TaskRegistry {
                closed: false,
                max_tasks,
                max_bytes,
                reserved_bytes: 0,
                workers: JoinSet::new(),
                worker_controls: BTreeMap::new(),
                pending_observations: Vec::new(),
                worker_failed: false,
                unobserved_results: false,
                #[cfg(test)]
                completion_gate: None,
            })),
        }
    }

    pub(crate) fn spawn<T, Work>(
        &self,
        reserved_bytes: u64,
        work: Work,
    ) -> Result<ScopedTask<T>, ScopedTaskError>
    where
        T: Send + 'static,
        Work: Future<Output = Result<T, PipelineFailure>> + Send + 'static,
    {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| ScopedTaskError::RegistryUnavailable)?;
        registry.reap_finished();
        let next_reserved = registry.next_reservation(reserved_bytes)?;

        let (sender, receiver) = oneshot::channel();
        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = observed.clone();
        let worker = tokio::spawn(work);
        let worker_id = worker.id();
        let abort = worker.abort_handle();
        let worker_abort = abort.clone();
        registry.worker_controls.insert(worker_id, abort.clone());
        #[cfg(test)]
        let completion_gate = registry.completion_gate.take();
        registry.workers.spawn(async move {
            let abort_on_drop = AbortOnDrop(worker_abort);
            let joined = worker.await;
            drop(abort_on_drop);
            let (outcome, result) = match joined {
                Ok(Ok(output)) => (WorkerOutcome::Succeeded, Ok(output)),
                Ok(Err(failure)) => (
                    WorkerOutcome::Failed,
                    Err(ScopedTaskError::WorkerFailed {
                        stage: failure.stage(),
                    }),
                ),
                Err(error) if error.is_cancelled() => {
                    (WorkerOutcome::Cancelled, Err(ScopedTaskError::Cancelled))
                }
                Err(_) => (WorkerOutcome::Failed, Err(ScopedTaskError::WorkerPanicked)),
            };
            let delivered = sender.send(result).is_ok();
            #[cfg(test)]
            if let Some(completion_gate) = completion_gate {
                let _ = completion_gate.await;
            }
            WorkerCompletion {
                worker_id,
                outcome,
                delivered,
                observed: worker_observed,
                reserved_bytes,
            }
        });
        registry.reserved_bytes = next_reserved;
        Ok(ScopedTask {
            receiver,
            observed,
            abort,
        })
    }

    pub(crate) fn record_stage_failure(&self) -> Result<(), ScopedTaskError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| ScopedTaskError::RegistryUnavailable)?;
        registry.worker_failed = true;
        Ok(())
    }

    pub(crate) fn probe_admission(&self, reserved_bytes: u64) -> Result<(), ScopedTaskError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| ScopedTaskError::RegistryUnavailable)?;
        registry.reap_finished();
        registry.next_reservation(reserved_bytes).map(|_| ())
    }

    /// Reap the existing observer tasks after a stage's local joins are drained.
    /// Result delivery precedes observer completion; reservations stay charged
    /// until both completion and output acknowledgement have been observed.
    pub(crate) async fn reap_workers(&self) -> Result<(), ScopedTaskError> {
        poll_fn(|context| {
            let Ok(mut registry) = self.registry.lock() else {
                return Poll::Ready(Err(ScopedTaskError::RegistryUnavailable));
            };
            registry.release_observed_results();
            loop {
                match registry.workers.poll_join_next(context) {
                    Poll::Ready(Some(Ok(completion))) => registry.record_completion(completion),
                    Poll::Ready(Some(Err(_))) => registry.worker_failed = true,
                    Poll::Ready(None) => {
                        registry.release_observed_results();
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Pending => {
                        registry.release_observed_results();
                        return Poll::Pending;
                    }
                }
            }
        })
        .await
    }

    #[cfg(test)]
    pub(crate) fn gate_next_completion(
        &self,
        gate: oneshot::Receiver<()>,
    ) -> Result<(), ScopedTaskError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| ScopedTaskError::RegistryUnavailable)?;
        registry.completion_gate = Some(gate);
        Ok(())
    }

    pub(crate) fn max_tasks(&self) -> Result<usize, ScopedTaskError> {
        self.registry
            .lock()
            .map_err(|_| ScopedTaskError::RegistryUnavailable)
            .map(|registry| registry.max_tasks)
    }

    /// Close admission and reap without moving handles out of their owner.
    /// A timed-out or dropped drain therefore retains running blocking work.
    pub(crate) async fn close_abort_and_reap_retained(&self, deadline: Instant) -> ReapReport {
        self.close_and_abort();
        loop {
            if let Ok(mut registry) = self.registry.lock() {
                registry.reap_finished();
                let all_joined = registry.workers.is_empty();
                if all_joined || Instant::now() >= deadline {
                    return ReapReport {
                        active_tasks: registry.workers.len(),
                        all_joined,
                        worker_failed: registry.worker_failed,
                        unobserved_results: registry.unobserved_results
                            || registry
                                .pending_observations
                                .iter()
                                .any(|pending| !pending.observed.load(Ordering::Acquire)),
                    };
                }
            } else {
                return ReapReport {
                    active_tasks: 0,
                    all_joined: false,
                    worker_failed: true,
                    unobserved_results: false,
                };
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

impl TaskRegistry {
    fn next_reservation(&self, reserved_bytes: u64) -> Result<u64, ScopedTaskError> {
        if self.closed {
            return Err(ScopedTaskError::ScopeClosed);
        }
        if reserved_bytes == 0 || reserved_bytes > self.max_bytes {
            return Err(ScopedTaskError::InvalidByteReservation);
        }
        if self.tracked_tasks() >= self.max_tasks {
            return Err(ScopedTaskError::TaskCapacityExceeded);
        }
        let next_reserved = self
            .reserved_bytes
            .checked_add(reserved_bytes)
            .ok_or(ScopedTaskError::ByteCapacityExceeded)?;
        if next_reserved > self.max_bytes {
            Err(ScopedTaskError::ByteCapacityExceeded)
        } else {
            Ok(next_reserved)
        }
    }

    fn tracked_tasks(&self) -> usize {
        self.workers.len() + self.pending_observations.len()
    }

    fn reap_finished(&mut self) {
        self.release_observed_results();
        while let Some(joined) = self.workers.try_join_next() {
            match joined {
                Ok(completion) => self.record_completion(completion),
                Err(_) => self.worker_failed = true,
            }
        }
        self.release_observed_results();
    }

    fn record_completion(&mut self, completion: WorkerCompletion) {
        self.worker_controls.remove(&completion.worker_id);
        match completion.outcome {
            WorkerOutcome::Failed => {
                self.worker_failed = true;
                self.release_bytes(completion.reserved_bytes);
            }
            WorkerOutcome::Cancelled => self.release_bytes(completion.reserved_bytes),
            WorkerOutcome::Succeeded if !completion.delivered => {
                self.unobserved_results = true;
                self.release_bytes(completion.reserved_bytes);
            }
            WorkerOutcome::Succeeded if completion.observed.load(Ordering::Acquire) => {
                self.release_bytes(completion.reserved_bytes);
            }
            WorkerOutcome::Succeeded => {
                self.pending_observations.push(PendingObservation {
                    observed: completion.observed,
                    reserved_bytes: completion.reserved_bytes,
                });
            }
        }
    }

    fn release_observed_results(&mut self) {
        let mut pending = Vec::with_capacity(self.pending_observations.len());
        let observations = std::mem::take(&mut self.pending_observations);
        for observation in observations {
            if observation.observed.load(Ordering::Acquire) {
                self.release_bytes(observation.reserved_bytes);
            } else {
                pending.push(observation);
            }
        }
        self.pending_observations = pending;
    }

    fn release_bytes(&mut self, released: u64) {
        if let Some(remaining) = self.reserved_bytes.checked_sub(released) {
            self.reserved_bytes = remaining;
        } else {
            self.reserved_bytes = 0;
            self.worker_failed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::{
        future::pending,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use tokio::sync::oneshot;

    use super::*;

    const TEST_TASK_CAPACITY: usize = 2;
    const SINGLE_TASK_CAPACITY: usize = 1;
    const TEST_BYTE_CAPACITY: u64 = 8;
    const SMALL_BYTE_CAPACITY: u64 = 4;
    const ONE_BYTE: u64 = 1;
    const TWO_BYTES: u64 = 2;
    const THREE_BYTES: u64 = 3;
    const FOUR_BYTES: u64 = 4;
    const LARGE_TEST_BYTE_CAPACITY: u64 = 1_024;
    const REAP_TIMEOUT: Duration = Duration::from_secs(1);

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn reaping_workers_preserves_unread_and_unacknowledged_output_reservations() {
        let scope = TaskScope::new(SINGLE_TASK_CAPACITY, SMALL_BYTE_CAPACITY);
        let task = scope
            .spawn(FOUR_BYTES, async {
                Ok::<_, PipelineFailure>(vec![0_u8; 4])
            })
            .unwrap_or_else(|error| panic!("worker admission failed: {error}"));
        assert!(scope.reap_workers().await.is_ok());
        assert_eq!(
            scope.probe_admission(ONE_BYTE),
            Err(ScopedTaskError::TaskCapacityExceeded)
        );
        let (output, reservation) = task
            .join_retaining_reservation()
            .await
            .unwrap_or_else(|error| panic!("worker result failed: {error}"));
        assert_eq!(
            scope.probe_admission(ONE_BYTE),
            Err(ScopedTaskError::TaskCapacityExceeded)
        );
        drop(output);
        reservation.acknowledge();
        assert_eq!(scope.probe_admission(FOUR_BYTES), Ok(()));
        let report = scope
            .close_abort_and_reap_retained(Instant::now() + REAP_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retained_drain_waits_for_actual_blocking_worker_after_cancellation() {
        let scope = TaskScope::new(TEST_TASK_CAPACITY, LARGE_TEST_BYTE_CAPACITY);
        let (started, receiver) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let task = scope
            .spawn(ONE_BYTE, async move {
                let released = tokio::task::block_in_place(|| {
                    let _ = started.send(());
                    blocked.recv_timeout(Duration::from_secs(10)).is_ok()
                });
                assert!(released, "blocking worker release was not delivered");
                tokio::task::yield_now().await;
                Ok::<(), PipelineFailure>(())
            })
            .unwrap_or_else(|error| panic!("worker admission failed: {error}"));
        assert!(tokio::time::timeout(REAP_TIMEOUT, receiver).await.is_ok());
        let incomplete = scope
            .close_abort_and_reap_retained(Instant::now() + Duration::from_millis(25))
            .await;
        assert!(!incomplete.all_joined);
        assert_eq!(incomplete.active_tasks, 1);
        assert!(release.send(()).is_ok());
        let complete = scope
            .close_abort_and_reap_retained(Instant::now() + REAP_TIMEOUT)
            .await;
        assert!(complete.all_joined);
        assert!(!complete.worker_failed);
        assert_eq!(task.join().await, Err(ScopedTaskError::Cancelled));
    }

    #[tokio::test]
    async fn closing_scope_aborts_and_joins_a_detached_registered_worker() {
        let scope = TaskScope::new(TEST_TASK_CAPACITY, LARGE_TEST_BYTE_CAPACITY);
        let dropped = Arc::new(AtomicBool::new(false));
        let worker_drop = dropped.clone();
        let (started, started_receiver) = oneshot::channel();
        let task = match scope.spawn(ONE_BYTE, async move {
            let _drop_flag = DropFlag(worker_drop);
            let _ = started.send(());
            pending::<Result<(), PipelineFailure>>().await
        }) {
            Ok(task) => task,
            Err(error) => panic!("registered worker did not spawn: {error}"),
        };
        assert!(started_receiver.await.is_ok());
        let report = scope
            .close_abort_and_reap_retained(Instant::now() + REAP_TIMEOUT)
            .await;
        assert_eq!(report.active_tasks, 0);
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(task.join().await, Err(ScopedTaskError::Cancelled));
        assert_matches!(
            scope
                .spawn(ONE_BYTE, async { Ok::<(), PipelineFailure>(()) })
                .err(),
            Some(ScopedTaskError::ScopeClosed)
        );
    }

    #[tokio::test]
    async fn admission_caps_tasks_and_bytes_without_an_unbounded_queue() {
        let task_limited = TaskScope::new(SINGLE_TASK_CAPACITY, TEST_BYTE_CAPACITY);
        let first = task_limited.spawn(FOUR_BYTES, pending::<Result<(), PipelineFailure>>());
        assert!(first.is_ok());
        assert_matches!(
            task_limited
                .spawn(ONE_BYTE, async { Ok::<(), PipelineFailure>(()) })
                .err(),
            Some(ScopedTaskError::TaskCapacityExceeded)
        );
        let report = task_limited
            .close_abort_and_reap_retained(Instant::now() + REAP_TIMEOUT)
            .await;
        assert!(report.all_joined);

        let byte_limited = TaskScope::new(TEST_TASK_CAPACITY, SMALL_BYTE_CAPACITY);
        let first = byte_limited.spawn(THREE_BYTES, pending::<Result<(), PipelineFailure>>());
        assert!(first.is_ok());
        assert_matches!(
            byte_limited
                .spawn(TWO_BYTES, async { Ok::<(), PipelineFailure>(()) })
                .err(),
            Some(ScopedTaskError::ByteCapacityExceeded)
        );
        assert_matches!(
            byte_limited
                .spawn(0, async { Ok::<(), PipelineFailure>(()) })
                .err(),
            Some(ScopedTaskError::InvalidByteReservation)
        );
        let report = byte_limited
            .close_abort_and_reap_retained(Instant::now() + REAP_TIMEOUT)
            .await;
        assert!(report.all_joined);
    }

    #[tokio::test]
    async fn dropped_worker_failure_remains_visible_to_publication_gate() {
        let scope = TaskScope::new(TEST_TASK_CAPACITY, TEST_BYTE_CAPACITY);
        let task = match scope.spawn(ONE_BYTE, async {
            Err::<(), PipelineFailure>(PipelineFailure::new(PipelineStage::Parse))
        }) {
            Ok(task) => task,
            Err(error) => panic!("failing worker did not spawn: {error}"),
        };
        drop(task);
        tokio::task::yield_now().await;
        let report = scope
            .close_abort_and_reap_retained(Instant::now() + REAP_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }
}
