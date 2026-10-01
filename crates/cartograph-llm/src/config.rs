use std::{env, path::Path, time::Duration};

use secrecy::SecretString;
use url::Url;

use crate::credential::TierCredential;
use crate::{EmbeddingError, ProjectLlmTier, ProjectLlmTierConfig, load_project_llm_tier};

/// Optional API key for the OpenAI-compatible embedding endpoint.
pub const EMBEDDING_API_KEY_ENV: &str = "CARTOGRAPH_EMBEDDING_API_KEY";
/// Base URL or complete `/v1/embeddings` URL for the embedding endpoint.
pub const EMBEDDING_ENDPOINT_ENV: &str = "CARTOGRAPH_EMBEDDING_ENDPOINT";
/// Exact model identifier sent to the embedding endpoint.
pub const EMBEDDING_MODEL_ENV: &str = "CARTOGRAPH_EMBEDDING_MODEL";
/// Hard request deadline in milliseconds.
pub const EMBEDDING_TIMEOUT_MS_ENV: &str = "CARTOGRAPH_EMBEDDING_TIMEOUT_MS";
/// Maximum inputs admitted to one HTTP request.
pub const EMBEDDING_MAX_BATCH_ENV: &str = "CARTOGRAPH_EMBEDDING_MAX_BATCH";
/// Maximum aggregate UTF-8 input bytes in one request.
pub const EMBEDDING_MAX_INPUT_BYTES_ENV: &str = "CARTOGRAPH_EMBEDDING_MAX_INPUT_BYTES";
/// Maximum HTTP response bytes read before cancellation.
pub const EMBEDDING_MAX_RESPONSE_BYTES_ENV: &str = "CARTOGRAPH_EMBEDDING_MAX_RESPONSE_BYTES";

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAXIMUM_TIMEOUT_MS: u64 = 600_000;
const MAXIMUM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAXIMUM_BATCH: usize = 32;
const MAXIMUM_BATCH: usize = 128;
const DEFAULT_MAXIMUM_INPUT_BYTES: usize = 2 * 1_024 * 1_024;
const MAXIMUM_INPUT_BYTES: usize = 16 * 1_024 * 1_024;
const DEFAULT_MAXIMUM_RESPONSE_BYTES: usize = 16 * 1_024 * 1_024;
const MAXIMUM_RESPONSE_BYTES: usize = 64 * 1_024 * 1_024;
const MAXIMUM_MODEL_BYTES: usize = 256;
const MAXIMUM_API_KEY_BYTES: usize = 8_192;
const PROJECT_CONFIG_FIELD: &str = ".cartograph/config.json embeddingLlm";

struct IntegerSetting<T> {
    key: &'static str,
    raw: Option<String>,
    default: T,
    maximum: T,
}

/// Validated HTTP/model/secret and resource policy for one embedding client.
#[derive(Clone)]
pub struct EmbeddingSettings {
    endpoint: Url,
    model: String,
    credential: TierCredential,
    request_timeout: Duration,
    maximum_batch: usize,
    maximum_input_bytes: usize,
    maximum_response_bytes: usize,
}

impl std::fmt::Debug for EmbeddingSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmbeddingSettings")
            .field("endpoint", &"<redacted>")
            .field("model", &self.model)
            .field("credential", &self.credential)
            .field("request_timeout", &self.request_timeout)
            .field("maximum_batch", &self.maximum_batch)
            .field("maximum_input_bytes", &self.maximum_input_bytes)
            .field("maximum_response_bytes", &self.maximum_response_bytes)
            .finish()
    }
}

impl EmbeddingSettings {
    /// Load optional embedding configuration. A completely absent endpoint/model
    /// means deterministic mode; a partial configuration fails closed.
    /// # Errors
    ///
    /// Returns an error for non-Unicode/partial endpoint-model configuration,
    /// invalid model/key text, or malformed/out-of-range numeric limits.
    pub fn try_from_env() -> Result<Option<Self>, EmbeddingError> {
        let endpoint = optional_env(EMBEDDING_ENDPOINT_ENV)?;
        let model = optional_env(EMBEDDING_MODEL_ENV)?;
        match (endpoint, model) {
            (None, None) => Ok(None),
            (Some(endpoint), Some(model)) => {
                let api_key = optional_env(EMBEDDING_API_KEY_ENV)?;
                let mut settings = Self::new(&endpoint, model, api_key)?;
                settings.request_timeout = Duration::from_millis(parse_u64(IntegerSetting {
                    key: EMBEDDING_TIMEOUT_MS_ENV,
                    raw: optional_env(EMBEDDING_TIMEOUT_MS_ENV)?,
                    default: DEFAULT_TIMEOUT_MS,
                    maximum: MAXIMUM_TIMEOUT_MS,
                })?);
                settings.maximum_batch = parse_usize(IntegerSetting {
                    key: EMBEDDING_MAX_BATCH_ENV,
                    raw: optional_env(EMBEDDING_MAX_BATCH_ENV)?,
                    default: DEFAULT_MAXIMUM_BATCH,
                    maximum: MAXIMUM_BATCH,
                })?;
                settings.maximum_input_bytes = parse_usize(IntegerSetting {
                    key: EMBEDDING_MAX_INPUT_BYTES_ENV,
                    raw: optional_env(EMBEDDING_MAX_INPUT_BYTES_ENV)?,
                    default: DEFAULT_MAXIMUM_INPUT_BYTES,
                    maximum: MAXIMUM_INPUT_BYTES,
                })?;
                settings.maximum_response_bytes = parse_usize(IntegerSetting {
                    key: EMBEDDING_MAX_RESPONSE_BYTES_ENV,
                    raw: optional_env(EMBEDDING_MAX_RESPONSE_BYTES_ENV)?,
                    default: DEFAULT_MAXIMUM_RESPONSE_BYTES,
                    maximum: MAXIMUM_RESPONSE_BYTES,
                })?;
                Ok(Some(settings))
            }
            _ => Err(EmbeddingError::IncompleteConfiguration),
        }
    }

    /// Prefer the explicit process environment, then load project embedding config.
    /// # Errors
    ///
    /// Returns an error if environment settings are invalid or the project
    /// embedding tier is unreadable, unsupported, or violates transport bounds.
    pub fn try_from_project(project_root: &Path) -> Result<Option<Self>, EmbeddingError> {
        if let Some(settings) = Self::try_from_env()? {
            return Ok(Some(settings));
        }
        let Some(config) =
            load_project_llm_tier(project_root, ProjectLlmTier::Embedding).map_err(|_| {
                EmbeddingError::InvalidConfiguration {
                    field: PROJECT_CONFIG_FIELD,
                }
            })?
        else {
            return Ok(None);
        };
        Self::from_project_config(&config).map(Some)
    }

    /// Build settings from one loaded project tier. A credential command is
    /// kept unresolved; the client runs it on the first request.
    /// # Errors
    ///
    /// Returns an error if the tier's endpoint or model violates the embedding
    /// transport bounds.
    pub fn from_project_config(config: &ProjectLlmTierConfig) -> Result<Self, EmbeddingError> {
        let mut settings = Self::new(config.endpoint(), config.model(), None)?;
        settings.credential = config.credential();
        if let Some(timeout_ms) = config.timeout_ms() {
            settings.request_timeout = Duration::from_millis(timeout_ms);
        }
        Ok(settings)
    }

    /// Validate an explicit endpoint/model/key boundary with production defaults.
    /// # Errors
    ///
    /// Returns an error if the endpoint is not permitted HTTPS/loopback HTTP,
    /// or model/API-key text is empty, oversized, or contains control characters.
    pub fn new(
        endpoint: &str,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self, EmbeddingError> {
        let endpoint = normalize_endpoint(endpoint)?;
        let model = model.into().trim().to_owned();
        if model.is_empty()
            || model.len() > MAXIMUM_MODEL_BYTES
            || model.chars().any(char::is_control)
        {
            return Err(EmbeddingError::InvalidConfiguration {
                field: EMBEDDING_MODEL_ENV,
            });
        }
        let api_key = match api_key {
            Some(value)
                if value.is_empty()
                    || value.len() > MAXIMUM_API_KEY_BYTES
                    || value.chars().any(char::is_control) =>
            {
                return Err(EmbeddingError::InvalidConfiguration {
                    field: EMBEDDING_API_KEY_ENV,
                });
            }
            Some(value) => Some(SecretString::from(value)),
            None => None,
        };
        Ok(Self {
            endpoint,
            model,
            credential: TierCredential::from_key(api_key),
            request_timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            maximum_batch: DEFAULT_MAXIMUM_BATCH,
            maximum_input_bytes: DEFAULT_MAXIMUM_INPUT_BYTES,
            maximum_response_bytes: DEFAULT_MAXIMUM_RESPONSE_BYTES,
        })
    }

    pub(crate) const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Exact model identifier sent over the API boundary.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    pub(crate) const fn credential(&self) -> &TierCredential {
        &self.credential
    }

    pub(crate) const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub(crate) fn connect_timeout(&self) -> Duration {
        self.request_timeout.min(MAXIMUM_CONNECT_TIMEOUT)
    }

    pub(crate) const fn maximum_batch(&self) -> usize {
        self.maximum_batch
    }

    pub(crate) const fn maximum_input_bytes(&self) -> usize {
        self.maximum_input_bytes
    }

    pub(crate) const fn maximum_response_bytes(&self) -> usize {
        self.maximum_response_bytes
    }
}

fn normalize_endpoint(raw: &str) -> Result<Url, EmbeddingError> {
    crate::endpoint::normalize_endpoint(raw, crate::endpoint::EndpointPath::Embeddings).map_err(
        |()| EmbeddingError::InvalidConfiguration {
            field: EMBEDDING_ENDPOINT_ENV,
        },
    )
}

fn optional_env(key: &'static str) -> Result<Option<String>, EmbeddingError> {
    match env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(EmbeddingError::InvalidConfiguration { field: key })
        }
    }
}

fn parse_u64(input: IntegerSetting<u64>) -> Result<u64, EmbeddingError> {
    input
        .raw
        .map_or(Some(input.default), |raw| raw.parse::<u64>().ok())
        .filter(|value| (1..=input.maximum).contains(value))
        .ok_or(EmbeddingError::InvalidConfiguration { field: input.key })
}

fn parse_usize(input: IntegerSetting<usize>) -> Result<usize, EmbeddingError> {
    input
        .raw
        .map_or(Some(input.default), |raw| raw.parse::<usize>().ok())
        .filter(|value| (1..=input.maximum).contains(value))
        .ok_or(EmbeddingError::InvalidConfiguration { field: input.key })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_ENDPOINT: &str = "http://localhost:8080";
    const IPV6_LOOPBACK_ENDPOINT: &str = "http://[::1]:8080";
    const CLOUD_V1_ENDPOINT: &str = "https://example.com/proxy/v1";
    const INSECURE_REMOTE_ENDPOINT: &str = "http://example.com";
    const CREDENTIAL_ENDPOINT: &str = "https://user:secret@example.com";
    const CLOUD_ENDPOINT: &str = "https://example.com";
    const PRIVATE_ENDPOINT: &str = "https://private-project.example.com/proxy";

    #[test]
    fn endpoint_policy_appends_embeddings_and_rejects_secret_or_insecure_urls() {
        let local = EmbeddingSettings::new(LOCAL_ENDPOINT, "model", None)
            .unwrap_or_else(|error| panic!("local endpoint failed: {error}"));
        assert_eq!(local.endpoint().path(), "/v1/embeddings");
        assert!(EmbeddingSettings::new(IPV6_LOOPBACK_ENDPOINT, "model", None).is_ok());
        let cloud = EmbeddingSettings::new(CLOUD_V1_ENDPOINT, "model", None)
            .unwrap_or_else(|error| panic!("cloud endpoint failed: {error}"));
        assert_eq!(cloud.endpoint().path(), "/proxy/v1/embeddings");
        assert!(EmbeddingSettings::new(INSECURE_REMOTE_ENDPOINT, "model", None).is_err());
        assert!(EmbeddingSettings::new(CREDENTIAL_ENDPOINT, "model", None).is_err());
        assert!(
            EmbeddingSettings::new(CLOUD_ENDPOINT, "model", Some("secret\nheader".to_owned()))
                .is_err()
        );
    }

    #[test]
    fn debug_output_redacts_endpoint_and_api_key() {
        let settings = EmbeddingSettings::new(
            PRIVATE_ENDPOINT,
            "safe-model",
            Some("private-api-key".to_owned()),
        )
        .unwrap_or_else(|error| panic!("settings failed: {error}"));
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains("private-project"));
        assert!(!rendered.contains("private-api-key"));
        assert!(rendered.contains("<redacted>"));
    }
}
