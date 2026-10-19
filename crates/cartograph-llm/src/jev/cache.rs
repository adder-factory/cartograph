//! Process-local exact-request cache. No prompts or credentials are retained.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use super::JevError;

const MAXIMUM_ENTRIES: usize = 256;
const MAXIMUM_BYTES: usize = 4 * 1024 * 1024;
const MAXIMUM_FLIGHTS: usize = 32;
const MAXIMUM_WAITERS: usize = 32;
const LIFETIME: Duration = Duration::from_secs(300);
type Response = Arc<[u8]>;
type FlightResult = Option<Result<Response, JevError>>;

struct Entry {
    key: [u8; 32],
    created: Instant,
    response: Response,
}

#[derive(Default)]
struct State {
    entries: VecDeque<Entry>,
    bytes: usize,
    flights: Vec<([u8; 32], watch::Receiver<FlightResult>)>,
}

pub(super) struct DecisionCache {
    state: Mutex<State>,
    waiting: Arc<Semaphore>,
}

impl Default for DecisionCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            waiting: Arc::new(Semaphore::new(MAXIMUM_WAITERS)),
        }
    }
}

pub(super) enum Access {
    Hit(Response),
    Lead(FlightOwner),
    Wait(FlightWaiter),
    Bypass,
}

impl DecisionCache {
    pub(super) fn shared() -> Arc<Self> {
        static CACHE: OnceLock<Arc<DecisionCache>> = OnceLock::new();
        CACHE.get_or_init(|| Arc::new(Self::default())).clone()
    }

    pub(super) fn access(self: &Arc<Self>, key: [u8; 32], now: Instant) -> Access {
        let Ok(mut state) = self.state.lock() else {
            return Access::Bypass;
        };
        state.expire(now);
        if let Some(position) = state.entries.iter().position(|entry| entry.key == key)
            && let Some(entry) = state.entries.remove(position)
        {
            let response = entry.response.clone();
            state.entries.push_back(entry);
            return Access::Hit(response);
        }
        if let Some((_, receiver)) = state.flights.iter().find(|(existing, _)| *existing == key) {
            return self
                .waiting
                .clone()
                .try_acquire_owned()
                .map_or(Access::Bypass, |permit| {
                    Access::Wait(FlightWaiter {
                        receiver: receiver.clone(),
                        _permit: permit,
                    })
                });
        }
        if state.flights.len() >= MAXIMUM_FLIGHTS {
            return Access::Bypass;
        }
        let (sender, receiver) = watch::channel(None);
        state.flights.push((key, receiver));
        Access::Lead(FlightOwner {
            cache: self.clone(),
            key,
            sender,
        })
    }
}

impl State {
    fn expire(&mut self, now: Instant) {
        self.entries.retain(|entry| {
            if now.saturating_duration_since(entry.created) >= LIFETIME {
                self.bytes = self.bytes.saturating_sub(entry.response.len());
                false
            } else {
                true
            }
        });
    }

    fn insert(&mut self, key: [u8; 32], response: Response, now: Instant) {
        self.expire(now);
        if response.len() > MAXIMUM_BYTES {
            return;
        }
        while self.entries.len() >= MAXIMUM_ENTRIES
            || self.bytes.saturating_add(response.len()) > MAXIMUM_BYTES
        {
            let Some(entry) = self.entries.pop_front() else {
                return;
            };
            self.bytes = self.bytes.saturating_sub(entry.response.len());
        }
        self.bytes += response.len();
        self.entries.push_back(Entry {
            key,
            created: now,
            response,
        });
    }
}

/// The requesting future owns the producer; dropping it closes the flight.
pub(super) struct FlightOwner {
    cache: Arc<DecisionCache>,
    key: [u8; 32],
    sender: watch::Sender<FlightResult>,
}

impl FlightOwner {
    pub(super) fn finish(self, result: Result<Response, JevError>, cacheable: bool) {
        if let Ok(response) = &result
            && cacheable
            && let Ok(mut state) = self.cache.state.lock()
        {
            state.insert(self.key, response.clone(), Instant::now());
        }
        self.sender.send_replace(Some(result));
    }
}

impl Drop for FlightOwner {
    fn drop(&mut self) {
        if let Ok(mut state) = self.cache.state.lock() {
            state.flights.retain(|(key, _)| *key != self.key);
        }
    }
}

pub(super) struct FlightWaiter {
    receiver: watch::Receiver<FlightResult>,
    _permit: OwnedSemaphorePermit,
}

impl FlightWaiter {
    pub(super) async fn wait(mut self) -> Result<Response, JevError> {
        self.receiver
            .wait_for(Option::is_some)
            .await
            .map_err(|_| JevError::EndpointUnavailable)?
            .as_ref()
            .cloned()
            .ok_or(JevError::EndpointUnavailable)?
    }
}

#[cfg(test)]
mod tests;
