use futures_util::StreamExt as _;
use reqwest::{StatusCode, header};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};

use crate::{
    EmbeddingBatch, EmbeddingError, EmbeddingModelIdentity, EmbeddingSettings, EmbeddingVector,
};

use crate::transport::{ModelTransport, RequestPriority, TransportSettings, model_transport};

/// Reusable bounded async client for one immutable endpoint/model configuration.
#[derive(Clone)]
pub struct OpenAiEmbeddingClient {
    settings: EmbeddingSettings,
    identity: EmbeddingModelIdentity,
    transport: std::sync::Arc<ModelTransport>,
}

impl std::fmt::Debug for OpenAiEmbeddingClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiEmbeddingClient")
            .field("settings", &self.settings)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl OpenAiEmbeddingClient {
    /// Build a redirect-free rustls client with explicit connect/request limits.
    /// # Errors
    ///
    /// Returns an error if no TLS crypto provider can be installed or the
    /// redirect-free bounded embedding HTTP client cannot be built.
    pub fn new(settings: EmbeddingSettings) -> Result<Self, EmbeddingError> {
        let transport = model_transport(TransportSettings {
            endpoint: settings.endpoint(),
            model: settings.model(),
            api_key: settings.api_key(),
            connect_timeout: settings.connect_timeout(),
            request_timeout: settings.request_timeout(),
        })
        .map_err(|()| EmbeddingError::ClientUnavailable)?;
        let identity = EmbeddingModelIdentity::from_endpoint_and_model(
            settings.endpoint().as_str(),
            settings.model(),
        );
        Ok(Self {
            settings,
            identity,
            transport,
        })
    }

    /// Stable endpoint/model identity without exposing the endpoint itself.
    #[must_use]
    pub const fn identity(&self) -> &EmbeddingModelIdentity {
        &self.identity
    }

    /// Maximum inputs admitted by this configured endpoint request.
    #[must_use]
    pub const fn maximum_batch(&self) -> usize {
        self.settings.maximum_batch()
    }

    /// Maximum aggregate UTF-8 input bytes admitted by one request.
    #[must_use]
    pub const fn maximum_input_bytes(&self) -> usize {
        self.settings.maximum_input_bytes()
    }

    /// Embed a bounded non-empty batch and restore response rows to input order.
    /// # Errors
    ///
    /// Returns an error if batch/text/header bounds fail, the endpoint rejects
    /// or is unavailable, or response bytes/JSON/indices/vectors violate the contract.
    pub async fn embed(&self, inputs: &[String]) -> Result<EmbeddingBatch, EmbeddingError> {
        self.embed_with_priority(inputs, RequestPriority::Foreground)
            .await
    }

    /// Embed a background batch while reserving endpoint capacity for queries.
    /// # Errors
    ///
    /// Returns the same errors as [`Self::embed`], including endpoint unavailability
    /// when the bounded queue or shared request deadline is exhausted.
    pub async fn embed_background(
        &self,
        inputs: &[String],
    ) -> Result<EmbeddingBatch, EmbeddingError> {
        self.embed_with_priority(inputs, RequestPriority::Background)
            .await
    }

    async fn embed_with_priority(
        &self,
        inputs: &[String],
        priority: RequestPriority,
    ) -> Result<EmbeddingBatch, EmbeddingError> {
        validate_inputs(inputs, &self.settings)?;
        let admission = self
            .transport
            .admit(priority, self.settings.request_timeout())
            .await
            .map_err(|()| EmbeddingError::EndpointUnavailable)?;
        let request = EmbeddingRequest {
            model: self.settings.model(),
            input: inputs,
            encoding_format: "float",
        };
        let mut builder = self
            .transport
            .client
            .post(self.settings.endpoint().clone())
            .timeout(
                admission
                    .remaining()
                    .map_err(|()| EmbeddingError::EndpointUnavailable)?,
            )
            .json(&request);
        if let Some(api_key) = self.settings.api_key() {
            let value = format!("Bearer {}", api_key.expose_secret());
            let value = header::HeaderValue::from_str(&value).map_err(|_| {
                EmbeddingError::InvalidConfiguration {
                    field: crate::EMBEDDING_API_KEY_ENV,
                }
            })?;
            builder = builder.header(header::AUTHORIZATION, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|_| EmbeddingError::EndpointUnavailable)?;
        if response.status() != StatusCode::OK {
            return Err(EmbeddingError::BackendRejected);
        }
        let maximum_response_bytes = u64::try_from(self.settings.maximum_response_bytes())
            .map_err(|_| EmbeddingError::ResponseLimit)?;
        if response
            .content_length()
            .is_some_and(|length| length > maximum_response_bytes)
        {
            return Err(EmbeddingError::ResponseLimit);
        }
        let body = read_bounded_body(response, self.settings.maximum_response_bytes()).await?;
        decode_response(&body, inputs.len(), self.identity.clone())
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingRow>,
}

#[derive(Deserialize)]
struct EmbeddingRow {
    index: usize,
    embedding: Vec<f32>,
}

fn validate_inputs(inputs: &[String], settings: &EmbeddingSettings) -> Result<(), EmbeddingError> {
    if inputs.is_empty() || inputs.len() > settings.maximum_batch() {
        return Err(EmbeddingError::RequestLimit);
    }
    let mut bytes = 0_usize;
    for input in inputs {
        if input.is_empty() || input.contains('\0') {
            return Err(EmbeddingError::RequestLimit);
        }
        bytes = bytes
            .checked_add(input.len())
            .ok_or(EmbeddingError::RequestLimit)?;
        if bytes > settings.maximum_input_bytes() {
            return Err(EmbeddingError::RequestLimit);
        }
    }
    Ok(())
}

async fn read_bounded_body(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<Vec<u8>, EmbeddingError> {
    let mut body = Vec::new();
    body.try_reserve(maximum_bytes.min(64 * 1_024))
        .map_err(|_| EmbeddingError::ResponseLimit)?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| EmbeddingError::EndpointUnavailable)?;
        let next = body
            .len()
            .checked_add(chunk.len())
            .ok_or(EmbeddingError::ResponseLimit)?;
        if next > maximum_bytes {
            return Err(EmbeddingError::ResponseLimit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn decode_response(
    body: &[u8],
    expected: usize,
    identity: EmbeddingModelIdentity,
) -> Result<EmbeddingBatch, EmbeddingError> {
    let response = serde_json::from_slice::<EmbeddingResponse>(body)
        .map_err(|_| EmbeddingError::InvalidResponse)?;
    if response.data.len() != expected {
        return Err(EmbeddingError::InvalidResponse);
    }
    let mut ordered = vec![None; expected];
    for row in response.data {
        let slot = ordered
            .get_mut(row.index)
            .ok_or(EmbeddingError::InvalidResponse)?;
        if slot.is_some() {
            return Err(EmbeddingError::InvalidResponse);
        }
        *slot = Some(EmbeddingVector::new(row.embedding)?);
    }
    let vectors = ordered
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(EmbeddingError::InvalidResponse)?;
    EmbeddingBatch::new(identity, vectors)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    use super::*;

    const FIRST_VECTOR: [f32; 3] = [1.0, 0.0, 0.5];
    const SECOND_VECTOR: [f32; 3] = [0.0, 1.0, 0.5];
    const FIXTURE_ENDPOINT: &str = "https://example.com/v1/embeddings";
    const OTHER_FIXTURE_ENDPOINT: &str = "https://other.example.com/v1/embeddings";
    const MAXIMUM_REQUEST_BYTES: usize = 64 * 1_024;
    const REQUEST_CHUNK_BYTES: usize = 4_096;
    const HTTP_HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";
    const DEFAULT_RESPONSE_LIMIT_BYTES: usize = 16 * 1_024 * 1_024;
    const OVERSIZED_RESPONSE_BYTES: usize = DEFAULT_RESPONSE_LIMIT_BYTES + 1;

    fn identity() -> EmbeddingModelIdentity {
        EmbeddingModelIdentity::from_endpoint_and_model(FIXTURE_ENDPOINT, "fixture-model")
    }

    #[test]
    fn response_rows_are_ordered_and_dimension_checked() {
        let body = serde_json::json!({
            "data": [
                {"index": 1, "embedding": SECOND_VECTOR},
                {"index": 0, "embedding": FIRST_VECTOR}
            ]
        });
        let body = serde_json::to_vec(&body)
            .unwrap_or_else(|error| panic!("fixture serialization failed: {error}"));
        let batch = decode_response(&body, 2, identity())
            .unwrap_or_else(|error| panic!("response decode failed: {error}"));
        assert_eq!(batch.dimension(), FIRST_VECTOR.len());
        assert_eq!(batch.vectors()[0].values(), FIRST_VECTOR);
        assert_eq!(batch.vectors()[1].values(), SECOND_VECTOR);
    }

    #[test]
    fn malformed_indexes_dimensions_and_nonfinite_values_fail_closed() {
        for body in [
            serde_json::json!({"data": [{"index": 0, "embedding": [1.0]}, {"index": 0, "embedding": [1.0]}]}),
            serde_json::json!({"data": [{"index": 0, "embedding": [1.0]}, {"index": 1, "embedding": [1.0, 2.0]}]}),
            serde_json::json!({"data": [{"index": 0, "embedding": [0.0]}, {"index": 1, "embedding": [1.0]}]}),
        ] {
            let body = serde_json::to_vec(&body)
                .unwrap_or_else(|error| panic!("fixture serialization failed: {error}"));
            assert!(decode_response(&body, 2, identity()).is_err());
        }
        assert!(EmbeddingVector::new(vec![f32::INFINITY]).is_err());
    }

    #[test]
    fn model_identity_is_stable_distinct_and_endpoint_safe() {
        let first = identity();
        assert_eq!(first, identity());
        let second = EmbeddingModelIdentity::from_endpoint_and_model(
            OTHER_FIXTURE_ENDPOINT,
            "fixture-model",
        );
        assert_ne!(first, second);
        let rendered = format!("{first:?}");
        assert!(!rendered.contains("example.com"));
    }

    #[tokio::test]
    async fn http_boundary_sends_bounded_openai_request_and_restores_order() {
        let response_body = serde_json::to_string(&serde_json::json!({
            "data": [
                {"index": 1, "embedding": SECOND_VECTOR},
                {"index": 0, "embedding": FIRST_VECTOR}
            ]
        }))
        .unwrap_or_else(|error| panic!("fixture response serialization failed: {error}"));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let (endpoint, request) = spawn_http_fixture(response);
        let settings = EmbeddingSettings::new(
            &endpoint,
            "fixture-model",
            Some("fixture-api-key".to_owned()),
        )
        .unwrap_or_else(|error| panic!("fixture settings failed: {error}"));
        let client = OpenAiEmbeddingClient::new(settings)
            .unwrap_or_else(|error| panic!("fixture client failed: {error}"));
        let batch = client
            .embed(&["first".to_owned(), "second".to_owned()])
            .await
            .unwrap_or_else(|error| panic!("fixture embedding failed: {error}"));
        let request = request
            .join()
            .unwrap_or_else(|_| panic!("fixture HTTP server panicked"));
        let request = String::from_utf8(request)
            .unwrap_or_else(|error| panic!("fixture request was not UTF-8: {error}"));

        assert!(request.starts_with("POST /v1/embeddings HTTP/1.1\r\n"));
        assert!(request.contains("authorization: Bearer fixture-api-key\r\n"));
        assert!(request.contains("\"model\":\"fixture-model\""));
        assert!(request.contains("\"input\":[\"first\",\"second\"]"));
        assert_eq!(batch.vectors()[0].values(), FIRST_VECTOR);
        assert_eq!(batch.vectors()[1].values(), SECOND_VECTOR);
    }

    #[tokio::test]
    async fn declared_oversized_response_is_rejected_before_body_read() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {OVERSIZED_RESPONSE_BYTES}\r\nConnection: close\r\n\r\n"
        );
        let (endpoint, request) = spawn_http_fixture(response);
        let settings = EmbeddingSettings::new(&endpoint, "fixture-model", None)
            .unwrap_or_else(|error| panic!("fixture settings failed: {error}"));
        let client = OpenAiEmbeddingClient::new(settings)
            .unwrap_or_else(|error| panic!("fixture client failed: {error}"));
        assert_eq!(
            client.embed(&["first".to_owned()]).await,
            Err(EmbeddingError::ResponseLimit)
        );
        request
            .join()
            .unwrap_or_else(|_| panic!("fixture HTTP server panicked"));
    }

    fn spawn_http_fixture(response: String) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("fixture bind failed: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("fixture address failed: {error}"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("fixture accept failed: {error}"));
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("fixture timeout failed: {error}"));
            let request = read_http_request(&mut stream);
            stream
                .write_all(response.as_bytes())
                .and_then(|()| stream.flush())
                .unwrap_or_else(|error| panic!("fixture response failed: {error}"));
            request
        });
        (format!("http://{address}"), handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; REQUEST_CHUNK_BYTES];
        loop {
            let read = stream
                .read(&mut chunk)
                .unwrap_or_else(|error| panic!("fixture request read failed: {error}"));
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            assert!(request.len() <= MAXIMUM_REQUEST_BYTES);
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
        content_length.is_some_and(|length| request.len() >= body_start.saturating_add(length))
    }
}
