use std::future::Future;

use cartograph_db::ReadyGeneration;
use tokio::{
    task::JoinHandle,
    time::{Instant, timeout_at},
};

use crate::PipelineFailure;

/// Pipeline work driven on its own Tokio task.
///
/// The monitor never polls the work inline. A monitor branch that awaited
/// shared progress state could otherwise stop polling a work future that already
/// owned that state's fair-lock permits, deadlocking both, and any long
/// synchronous section of the work would stall every other monitor branch.
/// Dropping this value aborts a still-running work task.
pub(super) struct SupervisedWork {
    handle: Option<JoinHandle<Result<ReadyGeneration, PipelineFailure>>>,
}

/// How a supervised work task ended on its own.
pub(super) enum WorkCompletion {
    /// The work future returned its result.
    Finished(Result<ReadyGeneration, PipelineFailure>),
    /// The work task panicked or was cancelled before returning a result.
    Interrupted,
}

impl SupervisedWork {
    pub(super) fn spawn<WorkFuture>(work: WorkFuture) -> Self
    where
        WorkFuture: Future<Output = Result<ReadyGeneration, PipelineFailure>> + Send + 'static,
    {
        Self {
            handle: Some(tokio::spawn(work)),
        }
    }

    /// Whether the work task has ended, joined or not.
    pub(super) fn has_ended(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Wait for the work task to end on its own.
    ///
    /// Cancel-safe: dropping this future leaves the task running and tracked.
    /// Once the task has ended it is no longer tracked, and further calls stay
    /// pending instead of polling a finished handle.
    pub(super) async fn completion(&mut self) -> WorkCompletion {
        let Some(handle) = self.handle.as_mut() else {
            return std::future::pending().await;
        };
        let joined = handle.await;
        self.handle = None;
        match joined {
            Ok(result) => WorkCompletion::Finished(result),
            Err(_) => WorkCompletion::Interrupted,
        }
    }

    /// Wait until `deadline` for the work to end; true once no work task remains.
    pub(super) async fn finished_by(&mut self, deadline: Instant) -> bool {
        self.handle.is_none() || timeout_at(deadline, self.completion()).await.is_ok()
    }

    /// Abort a still-running work task and wait until its future was dropped.
    ///
    /// Abort takes effect at the task's next yield, so a task inside a
    /// synchronous section is only reaped once that section ends. Returns true
    /// when no work task remains by `deadline`; a task still running then stays
    /// aborted and is reported as unreaped by the caller.
    pub(super) async fn abort_and_reap(&mut self, deadline: Instant) -> bool {
        let Some(handle) = self.handle.as_mut() else {
            return true;
        };
        handle.abort();
        let reaped = timeout_at(deadline, handle).await.is_ok();
        if reaped {
            self.handle = None;
        }
        reaped
    }
}

impl Drop for SupervisedWork {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
