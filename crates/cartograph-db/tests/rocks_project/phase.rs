use std::{
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

/// A cyclic test barrier which breaks and wakes all peers on a finite timeout.
pub(super) struct Barrier {
    participants: usize,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    epoch: u64,
    arrived: usize,
    broken: bool,
}

impl Barrier {
    pub(super) fn new(participants: usize) -> Self {
        Self {
            participants,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }

    pub(super) fn wait(&self) {
        let Ok(mut state) = self.state.lock() else {
            panic!("test barrier poisoned");
        };
        assert!(!state.broken, "test barrier broken");
        let epoch = state.epoch;
        state.arrived += 1;
        if state.arrived == self.participants {
            state.arrived = 0;
            state.epoch += 1;
            self.changed.notify_all();
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.epoch == epoch && !state.broken {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok((next, timed)) = self.changed.wait_timeout(state, remaining) else {
                panic!("test barrier poisoned");
            };
            state = next;
            if timed.timed_out() && state.epoch == epoch {
                state.broken = true;
                self.changed.notify_all();
            }
        }
        let broken = state.broken;
        // Release the mutex before panicking, allowing waiting peers to observe
        // the broken state and finish instead of hanging the test executable.
        drop(state);
        assert!(!broken, "test barrier deadline");
    }
}
