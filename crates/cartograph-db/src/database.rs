use thiserror::Error;

/// Expected persistence failures with credential-safe public messages.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StorageError {
    /// An exact immutable key was replayed with different contents.
    #[error("Cartograph immutable storage key conflicts with durable contents")]
    ImmutableConflict,
    /// A caller supplied an invalid bounded field.
    #[error("invalid {field} in Cartograph storage request")]
    InvalidInput {
        /// Stable field name; input contents are intentionally omitted.
        field: &'static str,
    },
    /// A database operation failed. Driver text is omitted because it may
    /// contain query data or deployment details.
    #[error("Cartograph native storage operation failed during {operation}")]
    DatabaseOperation {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// A bounded statement exceeded its native request deadline.
    #[error("Cartograph native storage operation timed out during {operation}")]
    StatementTimeout {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// A type-state transition disagreed with durable database state.
    #[error("generation cannot transition from {actual} to {requested}")]
    InvalidGenerationTransition {
        /// State currently stored in native storage.
        actual: String,
        /// State required by this operation.
        requested: &'static str,
    },
    /// A database row contains an identity or enum value this binary cannot parse.
    #[error("Cartograph native storage data violates the {field} domain contract")]
    CorruptStoredValue {
        /// Stable field name; raw stored contents are intentionally omitted.
        field: &'static str,
    },
    /// A generation token no longer has a corresponding durable row.
    #[error("generation does not exist in the project Cartograph store")]
    GenerationNotFound,
    /// A newer generation is already visible and cannot be replaced by an
    /// older ready token.
    #[error(
        "generation sequence {candidate_sequence} is not newer than current sequence {current_sequence}"
    )]
    StaleGeneration {
        /// Sequence carried by the ready token.
        candidate_sequence: i64,
        /// Sequence already published for the project.
        current_sequence: i64,
    },
    /// A generation mutation did not hold the current unexpired exact lease token.
    #[error("Cartograph generation mutation lost its exact native storage lease fence")]
    LeaseFenceLost,
    /// A read stage's expected publication is no longer the project pointer.
    #[error("Cartograph current generation changed during bounded retrieval")]
    CurrentGenerationChanged,
    /// A generation has no verified immutable BM25 relation.
    #[error("Cartograph generation search relation is unavailable")]
    SearchRelationUnavailable,
    /// A project reached the hard bound for retained generation search relations.
    #[error("Cartograph project search relation limit is reached")]
    SearchRelationLimitReached,
    /// A generation spill exhausted its explicit row or logical-byte quota.
    #[error("Cartograph native generation spill exceeded its {resource} limit")]
    GenerationSpillLimitReached {
        /// Stable quota name; source and database details are intentionally omitted.
        resource: &'static str,
    },
    /// A repeated spill batch disagreed with the already durable deterministic batch.
    #[error("Cartograph native generation spill batch conflicts with durable state")]
    GenerationSpillConflict,
    /// Cooperative cancellation stopped spill reduction or digest traversal.
    #[error("Cartograph native generation spill was cancelled")]
    GenerationSpillCancelled,
}

pub(crate) const fn validate_bounded_limit(limit: u16, maximum: u16) -> Result<(), StorageError> {
    if limit == 0 || limit > maximum {
        Err(StorageError::InvalidInput { field: "limit" })
    } else {
        Ok(())
    }
}

pub(crate) fn validate_json_object(
    value: &serde_json::Value,
    maximum_bytes: usize,
    field: &'static str,
) -> Result<(), StorageError> {
    let encoded = serde_json::to_vec(value).map_err(|_| StorageError::InvalidInput { field })?;
    if value.is_object() && encoded.len() <= maximum_bytes {
        Ok(())
    } else {
        Err(StorageError::InvalidInput { field })
    }
}
