use std::time::Duration;

use serde::{Deserialize, Serialize};

const MAXIMUM_NEIGHBORS: u16 = 50;

/// Atomic model-scoped similarity-cache rebuild for one current generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimilarityMaterializationReport {
    /// Number of models.
    pub models: u64,
    /// Number of source symbols.
    pub source_symbols: u64,
    /// Number of model symbol pairs.
    pub model_symbol_pairs: u64,
    /// Number of edges written.
    pub edges_written: u64,
    /// Number of neighbors per symbol.
    pub neighbors_per_symbol: u16,
    /// Minimum number of score millionths required by this request.
    pub minimum_score_millionths: u32,
}

/// Validated neighbor, score, and timeout policy for one similarity-cache rebuild.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimilarityMaterializationPolicy {
    pub(super) neighbors: u16,
    pub(super) minimum_score: f64,
    pub(super) statement_timeout: Duration,
}

impl SimilarityMaterializationPolicy {
    /// Validate the complete rebuild policy before opening a transaction.
    /// # Errors
    ///
    /// Returns an error if the deadline is invalid, `neighbors` is out of
    /// bounds, or `minimum_score` is non-finite/outside zero to one.
    pub fn new(
        neighbors: u16,
        minimum_score: f64,
        statement_timeout: Duration,
    ) -> Result<Self, SemanticStorageError> {
        validate_timeout(statement_timeout)?;
        if neighbors == 0
            || neighbors > MAXIMUM_NEIGHBORS
            || !minimum_score.is_finite()
            || !(0.0..=1.0).contains(&minimum_score)
        {
            return Err(invalid("similarity_materialization"));
        }
        Ok(Self {
            neighbors,
            minimum_score,
            statement_timeout,
        })
    }
}

use super::types::{SemanticStorageError, invalid, validate_timeout};
