//! One deadline owns credential resolution, cache waiting, admission and HTTP.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use cartograph_domain::{DecisionCacheOutcome, DecisionRequestMetrics};
use futures_util::StreamExt as _;
use reqwest::{StatusCode, header};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value;

use crate::{
    credential::{CredentialedRequest, bearer_header, send_credentialed},
    transport::RequestPriority,
};

use super::{
    JevClient, JevDecision, JevError, JevQuestion, MAXIMUM_RESPONSE_BYTES,
    cache::{Access, DecisionCache},
    decode_response, encode_request,
};

impl JevClient {
    /// Use the shared background allowance for bulk work. At most three
    /// background requests can occupy an origin's four active slots.
    #[must_use]
    pub fn for_background(mut self) -> Self {
        self.priority = RequestPriority::Background;
        self
    }

    /// Evaluate bounded questions under one timeout, including credential and
    /// cache waits. Project-scoped clients reuse exact validated responses for
    /// five minutes; standalone `from_config` clients bypass that cache.
    /// Dropping the future closes any owned flight without detached work.
    /// # Errors
    /// Returns a redacted credential, admission, HTTP or response-contract error.
    pub async fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> Result<JevDecision, JevError> {
        let started = Instant::now();
        tokio::time::timeout(
            self.settings.timeout,
            self.decide_timed(state, questions, started),
        )
        .await
        .map_err(|_| JevError::EndpointUnavailable)?
    }

    async fn decide_timed(
        &self,
        state: &Value,
        questions: &BTreeMap<String, JevQuestion>,
        started: Instant,
    ) -> Result<JevDecision, JevError> {
        let body = encode_request(state, questions)?;
        let current = self
            .settings
            .credential
            .current()
            .await
            .map_err(JevError::CredentialUnavailable)?;
        let access = self
            .cache_key(&body, current.as_ref())
            .map_or(Access::Bypass, |key| {
                DecisionCache::shared().access(key, Instant::now())
            });
        let (owner, outcome) = match access {
            Access::Hit(response) => {
                return self.cached(&response, questions, started, DecisionCacheOutcome::Hit);
            }
            Access::Wait(waiter) => {
                let response = waiter.wait().await?;
                return self.cached(
                    &response,
                    questions,
                    started,
                    DecisionCacheOutcome::Coalesced,
                );
            }
            Access::Lead(owner) => (Some(owner), DecisionCacheOutcome::Miss),
            Access::Bypass => (None, DecisionCacheOutcome::Bypassed),
        };
        let attempts = AtomicU32::new(0);
        let response = self.request(body, current, started, &attempts).await;
        let result = response.and_then(|response| {
            decode_response(&response, questions).map(|decision| (response, decision))
        });
        let attempts = attempts.load(Ordering::Relaxed);
        if let Some(owner) = owner {
            // A renewed credential has a different cache identity; never store
            // its response under the rejected credential's key.
            owner.finish(
                result
                    .as_ref()
                    .map(|(body, _)| body.clone())
                    .map_err(Clone::clone),
                attempts == 1,
            );
        }
        let (_, mut decision) = result?;
        self.account(&mut decision, started, outcome, attempts);
        Ok(decision)
    }

    fn cached(
        &self,
        response: &[u8],
        questions: &BTreeMap<String, JevQuestion>,
        started: Instant,
        outcome: DecisionCacheOutcome,
    ) -> Result<JevDecision, JevError> {
        let mut decision = decode_response(response, questions)?;
        decision.usage = None;
        self.account(&mut decision, started, outcome, 0);
        Ok(decision)
    }

    fn account(
        &self,
        decision: &mut JevDecision,
        started: Instant,
        cache: DecisionCacheOutcome,
        attempts: u32,
    ) {
        decision.metrics = Some(DecisionRequestMetrics {
            cache,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            provider_requests: attempts,
            input_tokens: decision.usage.and_then(|usage| usage.input_tokens),
            output_tokens: decision.usage.and_then(|usage| usage.output_tokens),
            background: matches!(self.priority, RequestPriority::Background),
        });
    }

    fn cache_key(&self, body: &[u8], current: Option<&SecretString>) -> Option<[u8; 32]> {
        let scope = self.settings.cache_scope?;
        let mut digest = blake3::Hasher::new_derive_key("cartograph.jev-decision-cache.v1");
        digest.update(&scope);
        self.settings.credential.hash_identity(&mut digest);
        for value in [
            self.settings.endpoint.as_str().as_bytes(),
            current?.expose_secret().as_bytes(),
            body,
        ] {
            digest.update(&value.len().to_le_bytes());
            digest.update(value);
        }
        Some(*digest.finalize().as_bytes())
    }

    async fn request(
        &self,
        body: Vec<u8>,
        current: Option<SecretString>,
        started: Instant,
        attempts: &AtomicU32,
    ) -> Result<Arc<[u8]>, JevError> {
        let remaining = self
            .settings
            .timeout
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(JevError::EndpointUnavailable)?;
        let admission = self
            .transport
            .admit(self.priority, remaining)
            .await
            .map_err(|()| JevError::EndpointUnavailable)?;
        let response = send_credentialed(CredentialedRequest {
            credential: &self.settings.credential,
            current,
            build: |key: Option<&SecretString>| {
                let authorization = key
                    .and_then(bearer_header)
                    .ok_or(JevError::ConfigurationUnavailable)?;
                let remaining = admission
                    .remaining()
                    .map_err(|()| JevError::EndpointUnavailable)?;
                attempts.fetch_add(1, Ordering::Relaxed);
                Ok(self
                    .transport
                    .client
                    .post(self.settings.endpoint.clone())
                    .timeout(remaining)
                    .header(header::AUTHORIZATION, authorization)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.clone()))
            },
        })
        .await?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(JevError::AuthenticationFailed);
            }
            status if status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529 => {
                return Err(JevError::RateLimited);
            }
            status if status.is_server_error() => return Err(JevError::EndpointUnavailable),
            _ => return Err(JevError::BackendRejected),
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAXIMUM_RESPONSE_BYTES as u64)
        {
            return Err(JevError::ResponseLimit);
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| JevError::EndpointUnavailable)?;
            if body.len().saturating_add(chunk.len()) > MAXIMUM_RESPONSE_BYTES {
                return Err(JevError::ResponseLimit);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body.into())
    }
}
