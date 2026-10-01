use std::{path::Path, time::Duration};

use futures_util::StreamExt as _;
use reqwest::{StatusCode, header};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::credential::{
    CredentialCommandError, CredentialedRequest, CredentialedRequestError, TierCredential,
    bearer_header, send_credentialed,
};
use crate::{CredentialCommandFailure, ProjectLlmTier, load_project_llm_tier};

const DEFAULT_TIMEOUT: Duration = Duration::from_mins(1);
const MAXIMUM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAXIMUM_QUERY_BYTES: usize = 128 * 1024;
const MAXIMUM_DOCUMENTS: usize = 128;
const MAXIMUM_DOCUMENT_BYTES: usize = 512 * 1024;
const MAXIMUM_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAXIMUM_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
use crate::transport::{ModelTransport, RequestPriority, TransportSettings, model_transport};

/// Validated Cohere-compatible `/v1/rerank` configuration.
#[derive(Clone)]
pub struct RerankSettings {
    endpoint: Url,
    model: String,
    credential: TierCredential,
    timeout: Duration,
}

impl std::fmt::Debug for RerankSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RerankSettings")
            .field("endpoint", &"<redacted>")
            .field("model", &self.model)
            .field("credential", &self.credential)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl RerankSettings {
    /// Load the optional project reranker. An absent block keeps reciprocal-rank
    /// fusion enabled without adding a model dependency.
    /// # Errors
    ///
    /// Returns an error if the project tier is invalid/unreadable or its
    /// endpoint cannot be normalized to a valid rerank URL.
    pub fn try_from_project(project_root: &Path) -> Result<Option<Self>, RerankError> {
        let Some(config) = load_project_llm_tier(project_root, ProjectLlmTier::Reranker)
            .map_err(|_| RerankError::InvalidConfiguration)?
        else {
            return Ok(None);
        };
        let endpoint = crate::endpoint::normalize_endpoint(
            config.endpoint(),
            crate::endpoint::EndpointPath::Rerank,
        )
        .map_err(|()| RerankError::InvalidConfiguration)?;
        let timeout = config
            .timeout_ms()
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        Ok(Some(Self {
            endpoint,
            model: config.model().to_owned(),
            credential: config.credential(),
            timeout,
        }))
    }

    /// Validated configured model identity without endpoint or credential material.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Input-ordered, finite reranker scores normalized to `[0, 1]`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RerankBatch {
    model: String,
    scores: Vec<f32>,
}

impl RerankBatch {
    #[must_use]
    /// Returns the model.
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    /// Returns the scores.
    pub fn scores(&self) -> &[f32] {
        &self.scores
    }
}

/// Bounded Cohere-compatible reranker client.
#[derive(Clone)]
pub struct OpenAiRerankClient {
    settings: RerankSettings,
    transport: std::sync::Arc<ModelTransport>,
}

impl OpenAiRerankClient {
    /// Creates a client after validating its transport configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if no TLS crypto provider can be installed or the
    /// redirect-free bounded rerank HTTP client cannot be built.
    pub fn new(settings: RerankSettings) -> Result<Self, RerankError> {
        let transport = model_transport(TransportSettings {
            endpoint: &settings.endpoint,
            model: &settings.model,
            credential: &settings.credential,
            connect_timeout: settings.timeout.min(MAXIMUM_CONNECT_TIMEOUT),
            request_timeout: settings.timeout,
        })
        .map_err(|()| RerankError::ClientUnavailable)?;
        Ok(Self {
            settings,
            transport,
        })
    }

    /// Validated configured model identity used for retrieval provenance.
    #[must_use]
    pub fn model(&self) -> &str {
        self.settings.model()
    }

    /// Score candidates and restore the response's rank-sorted rows to exact
    /// input order. Malformed, duplicate, or missing rows fail closed.
    /// # Errors
    ///
    /// Returns an error if query/document/header bounds fail, the endpoint
    /// rejects/times out, or response bytes/JSON/indices/scores are malformed.
    pub async fn rerank(
        &self,
        query: &str,
        documents: &[String],
    ) -> Result<RerankBatch, RerankError> {
        validate_input(query, documents)?;
        let current = self
            .settings
            .credential
            .current()
            .await
            .map_err(RerankError::credential_unavailable)?;
        let admission = self
            .transport
            .admit(RequestPriority::Foreground, self.settings.timeout)
            .await
            .map_err(|()| RerankError::EndpointUnavailable)?;
        let request = RerankRequest {
            model: &self.settings.model,
            query,
            documents,
            top_n: documents.len(),
        };
        let response = send_credentialed(CredentialedRequest {
            credential: &self.settings.credential,
            current,
            build: |key: Option<&SecretString>| {
                let mut builder = self
                    .transport
                    .client
                    .post(self.settings.endpoint.clone())
                    .timeout(
                        admission
                            .remaining()
                            .map_err(|()| RerankError::EndpointUnavailable)?,
                    )
                    .json(&request);
                if let Some(key) = key {
                    let authorization =
                        bearer_header(key).ok_or(RerankError::InvalidConfiguration)?;
                    builder = builder.header(header::AUTHORIZATION, authorization);
                }
                Ok(builder)
            },
        })
        .await?;
        if response.status() != StatusCode::OK {
            return Err(RerankError::BackendRejected);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAXIMUM_RESPONSE_BYTES as u64)
        {
            return Err(RerankError::ResponseLimit);
        }
        let body = read_bounded(response).await?;
        decode_response(&body, documents.len(), &self.settings.model)
    }
}

#[derive(Serialize)]
struct RerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [String],
    top_n: usize,
}

#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankRow>,
}

#[derive(Deserialize)]
struct RerankRow {
    index: usize,
    relevance_score: f32,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
/// Errors produced while processing rerank.
pub enum RerankError {
    #[error("Cartograph reranker configuration is invalid")]
    /// Configuration violates a required provider or resource contract.
    InvalidConfiguration,
    #[error("Cartograph reranker request exceeds configured bounds")]
    /// The request exceeded its declared size or item ceiling.
    RequestLimit,
    #[error("Cartograph reranker HTTP client is unavailable")]
    /// A safe bounded HTTP client could not be constructed.
    ClientUnavailable,
    #[error("Cartograph reranker endpoint is unavailable")]
    /// The configured endpoint could not complete the bounded request.
    EndpointUnavailable,
    #[error("Cartograph reranker credential command {failure}")]
    /// The tier's credential command produced no usable credential.
    CredentialUnavailable {
        /// Why the command produced no credential; never its output.
        failure: CredentialCommandFailure,
    },
    #[error("Cartograph reranker endpoint rejected the request")]
    /// The configured backend rejected the bounded request.
    BackendRejected,
    #[error("Cartograph reranker response exceeds configured bounds")]
    /// The response exceeded its declared size or item ceiling.
    ResponseLimit,
    #[error("Cartograph reranker endpoint returned an invalid response")]
    /// The backend response violates the expected bounded schema.
    InvalidResponse,
}

impl CredentialedRequestError for RerankError {
    fn credential_unavailable(error: CredentialCommandError) -> Self {
        Self::CredentialUnavailable {
            failure: error.failure(),
        }
    }

    fn endpoint_unavailable() -> Self {
        Self::EndpointUnavailable
    }
}

fn validate_input(query: &str, documents: &[String]) -> Result<(), RerankError> {
    if query.trim().is_empty()
        || query.len() > MAXIMUM_QUERY_BYTES
        || query.contains('\0')
        || documents.is_empty()
        || documents.len() > MAXIMUM_DOCUMENTS
    {
        return Err(RerankError::RequestLimit);
    }
    let mut total = query.len();
    for document in documents {
        if document.trim().is_empty()
            || document.len() > MAXIMUM_DOCUMENT_BYTES
            || document.contains('\0')
        {
            return Err(RerankError::RequestLimit);
        }
        total = total
            .checked_add(document.len())
            .filter(|value| *value <= MAXIMUM_INPUT_BYTES)
            .ok_or(RerankError::RequestLimit)?;
    }
    Ok(())
}

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, RerankError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| RerankError::EndpointUnavailable)?;
        let next = body
            .len()
            .checked_add(chunk.len())
            .filter(|value| *value <= MAXIMUM_RESPONSE_BYTES)
            .ok_or(RerankError::ResponseLimit)?;
        body.try_reserve(next.saturating_sub(body.len()))
            .map_err(|_| RerankError::ResponseLimit)?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn decode_response(body: &[u8], expected: usize, model: &str) -> Result<RerankBatch, RerankError> {
    let response =
        serde_json::from_slice::<RerankResponse>(body).map_err(|_| RerankError::InvalidResponse)?;
    if response.results.len() != expected {
        return Err(RerankError::InvalidResponse);
    }
    let raw = response
        .results
        .iter()
        .map(|row| row.relevance_score)
        .collect::<Vec<_>>();
    if raw.iter().any(|score| !score.is_finite()) {
        return Err(RerankError::InvalidResponse);
    }
    let normalized = normalize_scores(&raw);
    let mut scores = vec![None; expected];
    for (row, score) in response.results.into_iter().zip(normalized) {
        let slot = scores
            .get_mut(row.index)
            .ok_or(RerankError::InvalidResponse)?;
        if slot.replace(score).is_some() {
            return Err(RerankError::InvalidResponse);
        }
    }
    let scores = scores
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(RerankError::InvalidResponse)?;
    Ok(RerankBatch {
        model: model.to_owned(),
        scores,
    })
}

fn normalize_scores(scores: &[f32]) -> Vec<f32> {
    if scores.iter().all(|score| (0.0..=1.0).contains(score)) {
        return scores.to_vec();
    }
    let minimum = scores.iter().copied().fold(f32::INFINITY, f32::min);
    let maximum = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if minimum.total_cmp(&maximum).is_eq() {
        return vec![1.0; scores.len()];
    }
    scores
        .iter()
        .map(|score| (score - minimum) / (maximum - minimum))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpListener,
        thread,
        time::Duration,
    };

    use super::*;

    const FIXTURE_REQUEST_BYTES: usize = 64 * 1_024;
    const FIXTURE_REQUEST_CHUNK_BYTES: usize = 4 * 1_024;
    const HTTP_HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";
    const RERANK_CONFIG: &[u8] = br#"{"version":2,"llm":{"enabled":true,"rerankerLlm":{"provider":"openai-compat","endpoint":"http://127.0.0.1:8080/v1","model":"fixture-reranker","apiKey":"private-fixture","timeoutMs":3210}}}"#;
    const NORMALIZED_RERANK_ENDPOINT: &str = "http://127.0.0.1:8080/v1/rerank";

    #[test]
    fn response_is_input_ordered_and_raw_logits_are_normalized() {
        let body = br#"{"results":[{"index":1,"relevance_score":4.0},{"index":0,"relevance_score":-2.0}]}"#;
        let batch = decode_response(body, 2, "fixture")
            .unwrap_or_else(|error| panic!("fixture response failed: {error}"));
        assert_eq!(batch.scores(), &[0.0, 1.0]);
    }

    #[test]
    fn duplicate_missing_and_nonfinite_rows_fail_closed() {
        let duplicate =
            br#"{"results":[{"index":0,"relevance_score":0.5},{"index":0,"relevance_score":0.2}]}"#;
        assert_eq!(
            decode_response(duplicate, 2, "fixture"),
            Err(RerankError::InvalidResponse)
        );
        let nonfinite = br#"{"results":[{"index":0,"relevance_score":1e999}]}"#;
        assert_eq!(
            decode_response(nonfinite, 1, "fixture"),
            Err(RerankError::InvalidResponse)
        );
    }

    #[test]
    fn project_configuration_normalizes_the_endpoint_and_redacts_credentials() {
        let root = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("reranker project fixture failed: {error}"));
        std::fs::create_dir(root.path().join(".cartograph"))
            .unwrap_or_else(|error| panic!("reranker marker failed: {error}"));
        assert!(
            RerankSettings::try_from_project(root.path())
                .unwrap_or_else(|error| panic!("absent reranker failed: {error}"))
                .is_none()
        );
        std::fs::write(root.path().join(".cartograph/config.json"), RERANK_CONFIG)
            .unwrap_or_else(|error| panic!("reranker config write failed: {error}"));
        let settings = RerankSettings::try_from_project(root.path())
            .unwrap_or_else(|error| panic!("reranker config failed: {error}"))
            .unwrap_or_else(|| panic!("reranker config disappeared"));
        assert_eq!(settings.endpoint.as_str(), NORMALIZED_RERANK_ENDPOINT);
        assert_eq!(settings.model, "fixture-reranker");
        assert_eq!(settings.timeout, Duration::from_millis(3210));
        let rendered = format!("{settings:?}");
        assert!(rendered.contains("fixture-reranker"));
        assert!(!rendered.contains("127.0.0.1"));
        assert!(!rendered.contains("private-fixture"));
    }

    #[tokio::test]
    async fn http_boundary_sends_bounded_authenticated_request_and_restores_input_order() {
        let response_body =
            r#"{"results":[{"index":1,"relevance_score":0.9},{"index":0,"relevance_score":0.2}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let (endpoint, request) = spawn_http_fixture(response);
        let settings = RerankSettings {
            endpoint: Url::parse(&format!("{endpoint}/v1/rerank"))
                .unwrap_or_else(|error| panic!("reranker endpoint failed: {error}")),
            model: "fixture-reranker".to_owned(),
            credential: TierCredential::Static(SecretString::from("fixture-key")),
            timeout: Duration::from_secs(2),
        };
        let client = OpenAiRerankClient::new(settings)
            .unwrap_or_else(|error| panic!("reranker client failed: {error}"));
        let batch = client
            .rerank("rank this", &["first".to_owned(), "second".to_owned()])
            .await
            .unwrap_or_else(|error| panic!("reranker request failed: {error}"));
        assert_eq!(batch.model(), "fixture-reranker");
        assert_eq!(batch.scores(), &[0.2, 0.9]);

        let request = request
            .join()
            .unwrap_or_else(|_| panic!("reranker HTTP fixture panicked"));
        let request = String::from_utf8(request)
            .unwrap_or_else(|error| panic!("reranker request was not UTF-8: {error}"));
        assert!(request.starts_with("POST /v1/rerank HTTP/1.1\r\n"));
        assert!(request.contains("authorization: Bearer fixture-key\r\n"));
        assert!(request.contains("\"model\":\"fixture-reranker\""));
        assert!(request.contains("\"query\":\"rank this\""));
        assert!(request.contains("\"documents\":[\"first\",\"second\"]"));
        assert!(request.contains("\"top_n\":2"));
    }

    #[tokio::test]
    async fn request_and_response_limits_fail_before_unbounded_work() {
        for (query, documents) in [
            ("", vec!["document".to_owned()]),
            ("query\0", vec!["document".to_owned()]),
            ("query", Vec::new()),
            ("query", vec!["  ".to_owned()]),
            ("query", vec!["document\0".to_owned()]),
            (
                "query",
                (0..=MAXIMUM_DOCUMENTS)
                    .map(|index| format!("document-{index}"))
                    .collect(),
            ),
        ] {
            assert_eq!(
                validate_input(query, &documents),
                Err(RerankError::RequestLimit)
            );
        }
        assert_eq!(
            validate_input(
                &"q".repeat(MAXIMUM_QUERY_BYTES + 1),
                &["document".to_owned()]
            ),
            Err(RerankError::RequestLimit)
        );
        assert_eq!(
            validate_input("query", &["d".repeat(MAXIMUM_DOCUMENT_BYTES + 1)]),
            Err(RerankError::RequestLimit)
        );

        for (status, length, expected) in [
            (503_u16, 0_usize, RerankError::BackendRejected),
            (
                200_u16,
                MAXIMUM_RESPONSE_BYTES + 1,
                RerankError::ResponseLimit,
            ),
        ] {
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            );
            let (endpoint, request) = spawn_http_fixture(response);
            let settings = RerankSettings {
                endpoint: Url::parse(&format!("{endpoint}/v1/rerank"))
                    .unwrap_or_else(|error| panic!("reranker endpoint failed: {error}")),
                model: "fixture-reranker".to_owned(),
                credential: TierCredential::None,
                timeout: Duration::from_secs(2),
            };
            let client = OpenAiRerankClient::new(settings)
                .unwrap_or_else(|error| panic!("reranker client failed: {error}"));
            assert_eq!(
                client.rerank("query", &["document".to_owned()]).await,
                Err(expected)
            );
            request
                .join()
                .unwrap_or_else(|_| panic!("rejected reranker fixture panicked"));
        }
    }

    fn spawn_http_fixture(response: String) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("reranker fixture bind failed: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("reranker fixture address failed: {error}"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("reranker fixture accept failed: {error}"));
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("reranker fixture timeout failed: {error}"));
            let request = read_http_request(&mut stream);
            stream
                .write_all(response.as_bytes())
                .and_then(|()| stream.flush())
                .unwrap_or_else(|error| panic!("reranker fixture response failed: {error}"));
            request
        });
        (format!("http://{address}"), handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; FIXTURE_REQUEST_CHUNK_BYTES];
        loop {
            let read = stream
                .read(&mut chunk)
                .unwrap_or_else(|error| panic!("reranker fixture read failed: {error}"));
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            assert!(request.len() <= FIXTURE_REQUEST_BYTES);
            if complete_http_request(&request) {
                break;
            }
        }
        request
    }

    fn complete_http_request(request: &[u8]) -> bool {
        let Some(header_end) = request
            .windows(HTTP_HEADER_TERMINATOR.len())
            .position(|window| window == HTTP_HEADER_TERMINATOR)
        else {
            return false;
        };
        let body_start = header_end + HTTP_HEADER_TERMINATOR.len();
        let headers = std::str::from_utf8(&request[..header_end]).unwrap_or("");
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        });
        content_length.is_none_or(|length| request.len() >= body_start.saturating_add(length))
    }
}
