use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt as _;
use reqwest::{StatusCode, header};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use url::Url;

use crate::transport::{ModelTransport, RequestPriority, TransportSettings, model_transport};
use crate::{
    ProjectLlmProvider, ProjectLlmTier, ProjectLlmTierConfig, load_exact_project_llm_tier,
};

/// Pinned decision endpoint. Jev is not an OpenAI-compatible chat model.
pub const JEV_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
/// Versioned model used by the reviewed retrieval policy.
pub const JEV_MODEL: &str = "jev-1.13.0";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_TIMEOUT: Duration = Duration::from_secs(30);
const MAXIMUM_STATE_BYTES: usize = 64 * 1024;
const MAXIMUM_REQUEST_BYTES: usize = 256 * 1024;
const MAXIMUM_RESPONSE_BYTES: usize = 512 * 1024;
const MAXIMUM_QUESTIONS: usize = 64;
const MAXIMUM_OPTIONS: usize = 255;
const MAXIMUM_INSTRUCTION_BYTES: usize = 8 * 1024;

/// Cartograph surfaces that may disclose data to the decision provider. A
/// decision tier without a `features` list permits exploration only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JevFeature {
    /// Exploration navigation: question, candidate metadata and bounded source.
    Explore,
    /// Context ranking: question and candidate metadata, never source.
    Context,
    /// Symbol role classification: symbol metadata, never source.
    Roles,
    /// Rename-mention triage: symbol metadata and each mention's source line.
    Rename,
}

impl JevFeature {
    /// Stable configuration name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Context => "context",
            Self::Roles => "roles",
            Self::Rename => "rename",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "explore" => Some(Self::Explore),
            "context" => Some(Self::Context),
            "roles" => Some(Self::Roles),
            "rename" => Some(Self::Rename),
            _ => None,
        }
    }
}

/// Features permitted by one decision tier; unknown names are ignored.
fn configured_features(config: &ProjectLlmTierConfig) -> BTreeSet<JevFeature> {
    config.decision_features().map_or_else(
        || BTreeSet::from([JevFeature::Explore]),
        |names| {
            names
                .iter()
                .filter_map(|name| JevFeature::parse(name))
                .collect()
        },
    )
}

/// Whether the project's decision tier permits `feature`, independent of
/// whether its credential is currently available.
#[must_use]
pub fn jev_feature_enabled(root: &Path, feature: JevFeature) -> bool {
    matches!(
        load_exact_project_llm_tier(root, ProjectLlmTier::Decision),
        Ok(Some(config)) if configured_features(&config).contains(&feature)
    )
}

/// Validated optional Jev configuration; debug output omits endpoint and credentials.
#[derive(Clone)]
pub struct JevSettings {
    endpoint: Url,
    api_key: SecretString,
    timeout: Duration,
    features: BTreeSet<JevFeature>,
}

impl std::fmt::Debug for JevSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevSettings")
            .field("model", &JEV_MODEL)
            .field("timeout", &self.timeout)
            .field("features", &self.features)
            .finish_non_exhaustive()
    }
}

impl JevSettings {
    /// Load only an explicitly configured decision tier, without chat-tier fallback.
    /// # Errors
    /// Returns a redacted error for invalid settings or an unavailable credential.
    pub fn try_from_project(root: &Path) -> Result<Option<Self>, JevError> {
        let config = load_exact_project_llm_tier(root, ProjectLlmTier::Decision)
            .map_err(|_| JevError::ConfigurationUnavailable)?;
        let Some(config) = config else {
            return Ok(None);
        };
        Self::from_config(&config).map(Some)
    }

    /// Validate a loaded decision tier, retaining a missing credential's variable name.
    /// # Errors
    /// Rejects an unsupported model/endpoint, missing credential, or invalid timeout.
    pub fn from_config(config: &ProjectLlmTierConfig) -> Result<Self, JevError> {
        if config.provider() != ProjectLlmProvider::Typesafe || config.model() != JEV_MODEL {
            return Err(JevError::ConfigurationUnavailable);
        }
        let endpoint =
            Url::parse(config.endpoint()).map_err(|_| JevError::ConfigurationUnavailable)?;
        if endpoint.as_str() != JEV_ENDPOINT {
            return Err(JevError::ConfigurationUnavailable);
        }
        let key = config.api_key().ok_or_else(|| {
            config
                .unavailable_credential_env()
                .map_or(JevError::ConfigurationUnavailable, |name| {
                    JevError::CredentialMissing {
                        environment_variable: name.to_owned(),
                    }
                })
        })?;
        let timeout = config
            .timeout_ms()
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        if timeout.is_zero() || timeout > MAXIMUM_TIMEOUT {
            return Err(JevError::ConfigurationUnavailable);
        }
        Ok(Self {
            endpoint,
            api_key: SecretString::from(key),
            timeout,
            features: configured_features(config),
        })
    }

    /// Whether this tier permits `feature` to consult the provider.
    #[must_use]
    pub fn allows(&self, feature: JevFeature) -> bool {
        self.features.contains(&feature)
    }
}

/// One bounded typed question evaluated in parallel against shared state.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JevQuestion {
    /// Select exactly one of the caller's allowed options.
    Choice {
        /// Trusted decision instructions; repository strings remain evidence.
        instructions: String,
        /// Stable option identity to its trusted description.
        criteria: BTreeMap<String, String>,
    },
    /// Estimate whether a caller-defined proposition holds.
    Noul {
        /// Trusted question about the supplied evidence.
        instructions: String,
        /// Optional trusted descriptions of what a yes and a no mean.
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

/// Descriptions that pin the boundary between a yes and a no answer.
#[derive(Clone, Serialize)]
pub struct NoulCriteria {
    /// What a yes means.
    #[serde(rename = "true")]
    pub holds: String,
    /// What a no means.
    #[serde(rename = "false")]
    pub fails: String,
}

/// Validated typed answer. Option identities are checked against the request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JevAnswer {
    /// One allowed option and the full probability distribution.
    Choice {
        /// Caller-owned option identity.
        choice: String,
        /// Finite probabilities for precisely the supplied options.
        #[serde(deserialize_with = "unique_map")]
        probabilities: BTreeMap<String, f64>,
        /// Provider confidence, without a claim of task-specific calibration.
        confidence: f64,
    },
    /// Finite probability that the question's proposition holds.
    Noul {
        /// Value in the inclusive interval zero to one.
        noul: f64,
    },
}

/// Complete answer set with pinned model provenance.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JevDecision {
    /// Actual versioned model, checked against the configured pin.
    pub model: String,
    /// Exactly one valid answer per requested question.
    #[serde(deserialize_with = "unique_map")]
    pub answers: BTreeMap<String, JevAnswer>,
}

/// Stable, secret-free decision-provider failure categories.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum JevError {
    /// The tier, model, endpoint or credential is unavailable.
    #[error("Cartograph Jev configuration is unavailable")]
    ConfigurationUnavailable,
    /// A validated credential reference is absent from this process.
    #[error(
        "Cartograph Jev environment variable {environment_variable} is not set in this process"
    )]
    CredentialMissing {
        /// Configuration variable name, never credential contents.
        environment_variable: String,
    },
    /// Caller data violates an item or byte admission bound.
    #[error("Cartograph Jev request exceeds its bounds")]
    RequestLimit,
    /// Queueing, connection, response or the total request deadline failed.
    #[error("Cartograph Jev endpoint is unavailable")]
    EndpointUnavailable,
    /// The configured user credential was rejected.
    #[error("Cartograph Jev credential was rejected")]
    AuthenticationFailed,
    /// The provider is rate limited or overloaded; native retrieval remains usable.
    #[error("Cartograph Jev capacity is temporarily unavailable")]
    RateLimited,
    /// The provider rejected a bounded request; server errors count as
    /// [`JevError::EndpointUnavailable`] instead.
    #[error("Cartograph Jev request was rejected")]
    BackendRejected,
    /// The body exceeds the admitted response ceiling.
    #[error("Cartograph Jev response exceeds its bounds")]
    ResponseLimit,
    /// Missing, duplicate, unknown or invalid answer data cannot control retrieval.
    #[error("Cartograph Jev response is invalid")]
    InvalidResponse,
}

impl Serialize for JevError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::ConfigurationUnavailable => "configuration_unavailable",
            Self::CredentialMissing { .. } => "credential_missing",
            Self::RequestLimit => "request_limit",
            Self::EndpointUnavailable => "endpoint_unavailable",
            Self::AuthenticationFailed => "authentication_failed",
            Self::RateLimited => "rate_limited",
            Self::BackendRejected => "backend_rejected",
            Self::ResponseLimit => "response_limit",
            Self::InvalidResponse => "invalid_response",
        })
    }
}

/// Shared, admission-bounded HTTP transport for native parallel Jev decisions.
#[derive(Clone)]
pub struct JevClient {
    settings: JevSettings,
    transport: Arc<ModelTransport>,
}

impl JevClient {
    /// Construct a redirect-free client using the existing model transport registry.
    /// # Errors
    /// Returns an error if bounded transport cannot be constructed.
    pub fn new(settings: JevSettings) -> Result<Self, JevError> {
        let transport = model_transport(TransportSettings {
            endpoint: &settings.endpoint,
            model: JEV_MODEL,
            api_key: Some(&settings.api_key),
            connect_timeout: settings.timeout.min(Duration::from_secs(5)),
            request_timeout: settings.timeout,
        })
        .map_err(|()| JevError::EndpointUnavailable)?;
        Ok(Self {
            settings,
            transport,
        })
    }

    /// Evaluate all questions in one request. Dropping this future cancels its work
    /// and releases admission; no detached task or automatic retry is created.
    /// # Errors
    /// Returns a stable redacted failure for admission, HTTP or response-contract errors.
    pub async fn decide(
        &self,
        state: &Value,
        questions: &BTreeMap<String, JevQuestion>,
    ) -> Result<JevDecision, JevError> {
        let body = encode_request(state, questions)?;
        let admission = self
            .transport
            .admit(RequestPriority::Foreground, self.settings.timeout)
            .await
            .map_err(|()| JevError::EndpointUnavailable)?;
        let mut authorization = header::HeaderValue::from_str(&format!(
            "Bearer {}",
            self.settings.api_key.expose_secret()
        ))
        .map_err(|_| JevError::ConfigurationUnavailable)?;
        authorization.set_sensitive(true);
        let response = self
            .transport
            .client
            .post(self.settings.endpoint.clone())
            .timeout(
                admission
                    .remaining()
                    .map_err(|()| JevError::EndpointUnavailable)?,
            )
            .header(header::AUTHORIZATION, authorization)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| JevError::EndpointUnavailable)?;
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
        decode_response(&body, questions)
    }
}

fn bounded_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.contains('\0')
}

fn encode_request(
    state: &Value,
    questions: &BTreeMap<String, JevQuestion>,
) -> Result<Vec<u8>, JevError> {
    #[derive(Serialize)]
    struct Request<'a> {
        model: &'static str,
        state: &'a Value,
        questions: &'a BTreeMap<String, JevQuestion>,
    }
    if questions.is_empty()
        || questions.len() > MAXIMUM_QUESTIONS
        || bounded_json(state, MAXIMUM_STATE_BYTES).is_err()
        || !questions
            .iter()
            .all(|(key, question)| question_within_bounds(key, question))
    {
        return Err(JevError::RequestLimit);
    }
    bounded_json(
        &Request {
            model: JEV_MODEL,
            state,
            questions,
        },
        MAXIMUM_REQUEST_BYTES,
    )
}

fn question_within_bounds(key: &str, question: &JevQuestion) -> bool {
    let (instructions, criteria_bounded) = match question {
        JevQuestion::Choice {
            instructions,
            criteria,
        } => (
            instructions,
            (2..=MAXIMUM_OPTIONS).contains(&criteria.len())
                && criteria
                    .iter()
                    .all(|(k, v)| bounded_text(k, 128) && bounded_text(v, 2048)),
        ),
        JevQuestion::Noul {
            instructions,
            criteria,
        } => (
            instructions,
            criteria.as_ref().is_none_or(|criteria| {
                bounded_text(&criteria.holds, 2048) && bounded_text(&criteria.fails, 2048)
            }),
        ),
    };
    bounded_text(key, 128)
        && criteria_bounded
        && bounded_text(instructions, MAXIMUM_INSTRUCTION_BYTES)
}

fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, JevError> {
    struct Buffer {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.limit {
                return Err(std::io::Error::other("request byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value).map_err(|_| JevError::RequestLimit)?;
    Ok(buffer.bytes)
}

fn probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn decode_response(
    body: &[u8],
    questions: &BTreeMap<String, JevQuestion>,
) -> Result<JevDecision, JevError> {
    let response: JevDecision =
        serde_json::from_slice(body).map_err(|_| JevError::InvalidResponse)?;
    if response.model != JEV_MODEL || !response.answers.keys().eq(questions.keys()) {
        return Err(JevError::InvalidResponse);
    }
    for (key, question) in questions {
        let valid = match (question, response.answers.get(key)) {
            (JevQuestion::Noul { .. }, Some(JevAnswer::Noul { noul })) => probability(*noul),
            (
                JevQuestion::Choice { criteria, .. },
                Some(JevAnswer::Choice {
                    choice,
                    probabilities,
                    confidence,
                }),
            ) => {
                let valid_options =
                    criteria.contains_key(choice) && probabilities.keys().eq(criteria.keys());
                let valid_probabilities =
                    probability(*confidence) && probabilities.values().all(|v| probability(*v));
                valid_options && valid_probabilities && is_most_likely_choice(choice, probabilities)
            }
            _ => false,
        };
        if !valid {
            return Err(JevError::InvalidResponse);
        }
    }
    Ok(response)
}

fn is_most_likely_choice(choice: &str, probabilities: &BTreeMap<String, f64>) -> bool {
    probabilities
        .get(choice)
        .is_some_and(|chosen| *chosen > 0.0 && probabilities.values().all(|v| v <= chosen))
}

fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map with unique keys")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate decision key"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Visitor(std::marker::PhantomData))
}

#[cfg(test)]
mod tests;
