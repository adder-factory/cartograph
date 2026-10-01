use cartograph_domain::{ContentDigest, ModelId};
use serde::Serialize;
use thiserror::Error;

const MODEL_FINGERPRINT_DOMAIN: &[u8] = b"cartograph-v2-embedding-model-v1";
const MODEL_ID_BYTES: usize = 16;
// pgvector's `vector` HNSW operator class supports at most 2,000 dimensions.
const MAXIMUM_VECTOR_DIMENSION: usize = 2_000;

/// Stable model identity derived without retaining endpoint or credential text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbeddingModelIdentity {
    model_id: ModelId,
    fingerprint: ContentDigest,
    model: String,
}

impl EmbeddingModelIdentity {
    pub(crate) fn from_endpoint_and_model(endpoint: &str, model: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hash_field(&mut hasher, MODEL_FINGERPRINT_DOMAIN);
        hash_field(&mut hasher, endpoint.as_bytes());
        hash_field(&mut hasher, model.as_bytes());
        let bytes = *hasher.finalize().as_bytes();
        let mut uuid = [0_u8; MODEL_ID_BYTES];
        uuid.copy_from_slice(&bytes[..MODEL_ID_BYTES]);
        Self {
            model_id: ModelId::from_uuid_v8(uuid),
            fingerprint: ContentDigest::from_bytes(bytes),
            model: model.to_owned(),
        }
    }

    /// Deterministic model identifier used by PostgreSQL vector rows.
    #[must_use]
    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// Digest of normalized endpoint identity plus exact model name.
    #[must_use]
    pub const fn fingerprint(&self) -> &ContentDigest {
        &self.fingerprint
    }

    /// Exact non-secret model name.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// One finite, nonzero embedding vector with a bounded dimension.
#[derive(Clone, PartialEq, Serialize)]
pub struct EmbeddingVector(Vec<f32>);

impl EmbeddingVector {
    pub(crate) fn new(values: Vec<f32>) -> Result<Self, EmbeddingError> {
        if values.is_empty()
            || values.len() > MAXIMUM_VECTOR_DIMENSION
            || values.iter().any(|value| !value.is_finite())
        {
            return Err(EmbeddingError::InvalidResponse);
        }
        let magnitude = values
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>();
        if !magnitude.is_finite() || magnitude == 0.0 {
            return Err(EmbeddingError::InvalidResponse);
        }
        Ok(Self(values))
    }

    /// Vector dimension verified at the HTTP boundary.
    #[must_use]
    pub fn dimension(&self) -> usize {
        self.0.len()
    }

    /// Finite vector components in model response order.
    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.0
    }
}

impl std::fmt::Debug for EmbeddingVector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmbeddingVector")
            .field("dimension", &self.dimension())
            .finish_non_exhaustive()
    }
}

/// Ordered vectors returned for one ordered input batch.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EmbeddingBatch {
    model: EmbeddingModelIdentity,
    vectors: Vec<EmbeddingVector>,
}

impl EmbeddingBatch {
    pub(crate) fn new(
        model: EmbeddingModelIdentity,
        vectors: Vec<EmbeddingVector>,
    ) -> Result<Self, EmbeddingError> {
        let Some(dimension) = vectors.first().map(EmbeddingVector::dimension) else {
            return Err(EmbeddingError::InvalidResponse);
        };
        if vectors.iter().any(|vector| vector.dimension() != dimension) {
            return Err(EmbeddingError::InvalidResponse);
        }
        Ok(Self { model, vectors })
    }

    /// Stable endpoint/model identity for storage isolation.
    #[must_use]
    pub const fn model(&self) -> &EmbeddingModelIdentity {
        &self.model
    }

    /// Vectors in exact caller input order.
    #[must_use]
    pub fn vectors(&self) -> &[EmbeddingVector] {
        &self.vectors
    }

    /// Shared vector dimension for the complete response batch.
    #[must_use]
    pub fn dimension(&self) -> usize {
        self.vectors.first().map_or(0, EmbeddingVector::dimension)
    }
}

/// Secret- and payload-safe embedding boundary failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum EmbeddingError {
    /// Endpoint/model configuration is absent on only one required field.
    #[error("Cartograph embedding configuration is incomplete")]
    IncompleteConfiguration,
    /// One named configuration field is malformed or outside hard bounds.
    #[error("Cartograph embedding configuration field {field} is invalid")]
    InvalidConfiguration {
        /// Configuration field that violated its documented bound or format.
        field: &'static str,
    },
    /// Input count or UTF-8 bytes exceeded the configured admission policy.
    #[error("Cartograph embedding request exceeds configured bounds")]
    RequestLimit,
    /// The HTTP client could not be constructed safely.
    #[error("Cartograph embedding HTTP client is unavailable")]
    ClientUnavailable,
    /// The endpoint was unreachable or exceeded its request deadline.
    #[error("Cartograph embedding endpoint is unavailable")]
    EndpointUnavailable,
    /// The tier's credential command produced no usable credential.
    #[error("Cartograph embedding credential command {failure}")]
    CredentialUnavailable {
        /// Why the command produced no credential; never its output.
        failure: crate::CredentialCommandFailure,
    },
    /// The endpoint rejected the request without exposing its response body.
    #[error("Cartograph embedding endpoint rejected the request")]
    BackendRejected,
    /// A chunked or declared response exceeded the configured byte ceiling.
    #[error("Cartograph embedding response exceeds configured bounds")]
    ResponseLimit,
    /// JSON shape, indexes, dimensions, or numeric values were invalid.
    #[error("Cartograph embedding endpoint returned an invalid response")]
    InvalidResponse,
}

fn hash_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(value);
}
