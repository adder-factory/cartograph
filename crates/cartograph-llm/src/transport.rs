use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};
use url::Url;

use crate::credential::TierCredential;

const MAXIMUM_TRANSPORTS: usize = 32;
const MAXIMUM_ENDPOINTS: usize = 32;
const MAXIMUM_ACTIVE_REQUESTS: usize = 4;
const MAXIMUM_BACKGROUND_REQUESTS: usize = 3;
const MAXIMUM_WAITING_REQUESTS: usize = 32;
const MAXIMUM_WAITING_BACKGROUND_REQUESTS: usize = 16;
const USER_AGENT: &str = concat!("cartograph/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy)]
pub(crate) struct TransportSettings<'a> {
    pub endpoint: &'a Url,
    pub model: &'a str,
    pub credential: &'a TierCredential,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

#[derive(Clone, Copy)]
pub(crate) enum RequestPriority {
    Foreground,
    Background,
}

pub(crate) struct ModelTransport {
    pub client: reqwest::Client,
    admission: Arc<EndpointAdmission>,
}

struct EndpointAdmission {
    active: Arc<Semaphore>,
    background: Arc<Semaphore>,
    waiting: Arc<Semaphore>,
    waiting_background: Arc<Semaphore>,
}

impl Default for EndpointAdmission {
    fn default() -> Self {
        Self {
            active: Arc::new(Semaphore::new(MAXIMUM_ACTIVE_REQUESTS)),
            background: Arc::new(Semaphore::new(MAXIMUM_BACKGROUND_REQUESTS)),
            waiting: Arc::new(Semaphore::new(MAXIMUM_WAITING_REQUESTS)),
            waiting_background: Arc::new(Semaphore::new(MAXIMUM_WAITING_BACKGROUND_REQUESTS)),
        }
    }
}

pub(crate) struct AdmittedRequest {
    _active: OwnedSemaphorePermit,
    _background: Option<OwnedSemaphorePermit>,
    deadline: Instant,
}

impl AdmittedRequest {
    pub fn remaining(&self) -> Result<Duration, ()> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or(())
    }
}

impl ModelTransport {
    pub async fn admit(
        &self,
        priority: RequestPriority,
        timeout: Duration,
    ) -> Result<AdmittedRequest, ()> {
        let deadline = Instant::now().checked_add(timeout).ok_or(())?;
        let waiting_background = match priority {
            RequestPriority::Foreground => None,
            RequestPriority::Background => Some(
                self.admission
                    .waiting_background
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ())?,
            ),
        };
        let waiting = self
            .admission
            .waiting
            .clone()
            .try_acquire_owned()
            .map_err(|_| ())?;
        let admission = &self.admission;
        let request = tokio::time::timeout_at(deadline, async {
            let background = match priority {
                RequestPriority::Foreground => None,
                RequestPriority::Background => Some(
                    admission
                        .background
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| ())?,
                ),
            };
            let active = admission
                .active
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ())?;
            Ok(AdmittedRequest {
                _active: active,
                _background: background,
                deadline,
            })
        })
        .await
        .map_err(|_| ())?;
        drop(waiting);
        drop(waiting_background);
        request
    }
}

#[derive(Default)]
struct TransportRegistry {
    transports: Vec<([u8; 32], Arc<ModelTransport>)>,
    endpoints: Vec<([u8; 32], Arc<EndpointAdmission>)>,
}

pub(crate) fn model_transport(settings: TransportSettings<'_>) -> Result<Arc<ModelTransport>, ()> {
    static REGISTRY: OnceLock<Mutex<TransportRegistry>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Mutex::new(TransportRegistry::default()))
        .lock()
        .map_err(|_| ())?
        .get(settings)
}

impl TransportRegistry {
    fn get(&mut self, settings: TransportSettings<'_>) -> Result<Arc<ModelTransport>, ()> {
        let key = transport_key(&settings);
        if let Some((_, transport)) = self
            .transports
            .iter()
            .find(|(existing, _)| *existing == key)
        {
            return Ok(transport.clone());
        }
        crate::ensure_tls_crypto_provider().map_err(|_| ())?;
        let client = reqwest::Client::builder()
            .connect_timeout(settings.connect_timeout)
            .timeout(settings.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(MAXIMUM_ACTIVE_REQUESTS)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| ())?;
        if self.transports.len() == MAXIMUM_TRANSPORTS {
            self.transports.remove(0);
        }
        let endpoint_key =
            *blake3::hash(settings.endpoint.origin().ascii_serialization().as_bytes()).as_bytes();
        let admission = if let Some((_, admission)) =
            self.endpoints.iter().find(|(key, _)| *key == endpoint_key)
        {
            admission.clone()
        } else {
            if self.endpoints.len() == MAXIMUM_ENDPOINTS {
                let unused = self
                    .endpoints
                    .iter()
                    .position(|(_, admission)| Arc::strong_count(admission) == 1)
                    .ok_or(())?;
                self.endpoints.remove(unused);
            }
            self.endpoints.try_reserve(1).map_err(|_| ())?;
            let admission = Arc::new(EndpointAdmission::default());
            self.endpoints.push((endpoint_key, admission.clone()));
            admission
        };
        self.transports.try_reserve(1).map_err(|_| ())?;
        let transport = Arc::new(ModelTransport { client, admission });
        self.transports.push((key, transport.clone()));
        Ok(transport)
    }
}

fn transport_key(settings: &TransportSettings<'_>) -> [u8; 32] {
    let mut digest = blake3::Hasher::new_derive_key("cartograph.model-transport.v1");
    for text in [settings.endpoint.as_str(), settings.model] {
        digest.update(&text.len().to_le_bytes());
        digest.update(text.as_bytes());
    }
    settings.credential.hash_identity(&mut digest);
    digest.update(&settings.connect_timeout.as_nanos().to_le_bytes());
    digest.update(&settings.request_timeout.as_nanos().to_le_bytes());
    *digest.finalize().as_bytes()
}

#[cfg(test)]
mod tests;
