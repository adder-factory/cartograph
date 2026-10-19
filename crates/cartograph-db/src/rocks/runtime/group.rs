use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::{Result, RocksError, StorageRequest};

#[derive(Default)]
pub(super) struct StageGroup {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    closed: bool,
    pending: usize,
    failed: bool,
}

pub(super) struct Registration {
    group: Arc<StageGroup>,
    succeeded: bool,
}

impl StageGroup {
    pub(super) fn register(self: &Arc<Self>) -> Result<Registration> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| super::super::operation("staging_group"))?;
        if state.closed {
            return Err(RocksError::Transition);
        }
        state.pending = state.pending.checked_add(1).ok_or(RocksError::Limit {
            resource: "staging_jobs",
        })?;
        Ok(Registration {
            group: Arc::clone(self),
            succeeded: false,
        })
    }

    pub(super) async fn close_and_drain(
        &self,
        request: &StorageRequest,
        require_success: bool,
    ) -> Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| super::super::operation("staging_group"))?;
                state.closed = true;
                if state.pending == 0 {
                    if require_success && state.failed {
                        return Err(RocksError::Transition);
                    }
                    return request.check();
                }
            }
            tokio::select! {
                () = &mut changed => {},
                error = request.stopped() => return Err(error),
            }
        }
    }
}

impl Registration {
    pub(super) fn complete(&mut self, succeeded: bool) {
        self.succeeded = succeeded;
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut state) = self.group.state.lock() {
            state.failed |= !self.succeeded;
            state.pending = state.pending.saturating_sub(1);
        }
        self.group.changed.notify_waiters();
    }
}
