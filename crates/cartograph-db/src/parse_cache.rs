use std::time::Duration;

use cartograph_domain::{ContentDigest, NormalizedPath, ProjectId, SourceLanguage};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const CACHE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
mod native;
const CACHE_USAGE_TOUCH_INTERVAL: Duration = Duration::from_hours(1);
const MAXIMUM_RETAINED_CONTRACTS: u16 = 8;
const MAXIMUM_RETAINED_ROWS: u64 = 1_000_000;
const MAXIMUM_RETAINED_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAXIMUM_RETENTION_DELETIONS: u32 = 100_000;
const DEFAULT_RETAINED_CONTRACTS: u16 = 2;
const DEFAULT_RETAINED_ROWS: u64 = 20_000;
const DEFAULT_RETAINED_PAYLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_RETENTION_DELETIONS: u32 = 10_000;
const MAXIMUM_CACHE_BATCH_ENTRIES: usize = 256;
const MAXIMUM_CACHE_BATCH_LOAD_BYTES: i64 = 64 * 1024 * 1024;
const MAXIMUM_CACHE_BATCH_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
/// Maximum serialized facts retained for one immutable source file.
pub const MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Exact path-sensitive identity for one native extraction result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeParseCacheKey {
    project_id: ProjectId,
    extractor_contract_digest: ContentDigest,
    path: NormalizedPath,
    language: SourceLanguage,
    content_hash: ContentDigest,
    source_bytes: u64,
}

/// Source and extractor identity used to construct one native parse-cache key.
pub struct NativeParseCacheKeyInput {
    /// Stable project ID for this record.
    pub project_id: ProjectId,
    /// Digest-fenced extractor contract digest for this record.
    pub extractor_contract_digest: ContentDigest,
    /// Project-relative path for this record.
    pub path: NormalizedPath,
    /// Language for this record.
    pub language: SourceLanguage,
    /// Digest-fenced content hash for this record.
    pub content_hash: ContentDigest,
    /// Number of bytes used by the source.
    pub source_bytes: u64,
}

impl NativeParseCacheKey {
    #[must_use]
    /// Creates a validated native parse cache key.
    pub fn new(input: NativeParseCacheKeyInput) -> Self {
        let NativeParseCacheKeyInput {
            project_id,
            extractor_contract_digest,
            path,
            language,
            content_hash,
            source_bytes,
        } = input;
        Self {
            project_id,
            extractor_contract_digest,
            path,
            language,
            content_hash,
            source_bytes,
        }
    }

    #[must_use]
    /// Returns the project ID.
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    #[must_use]
    /// Returns the extractor contract digest.
    pub const fn extractor_contract_digest(&self) -> &ContentDigest {
        &self.extractor_contract_digest
    }

    #[must_use]
    /// Returns the path.
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    #[must_use]
    /// Returns the language.
    pub const fn language(&self) -> SourceLanguage {
        self.language
    }

    #[must_use]
    /// Returns the content hash.
    pub const fn content_hash(&self) -> &ContentDigest {
        &self.content_hash
    }

    #[must_use]
    /// Returns the source bytes.
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
}

/// Bounded serialized extraction facts loaded from native storage.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeParseCacheRecord {
    payload: Vec<u8>,
    payload_digest: ContentDigest,
    source_bytes: u64,
}

impl NativeParseCacheRecord {
    #[must_use]
    /// Returns the payload.
    pub const fn payload(&self) -> &[u8] {
        self.payload.as_slice()
    }

    #[must_use]
    /// Returns the payload digest.
    pub const fn payload_digest(&self) -> &ContentDigest {
        &self.payload_digest
    }

    #[must_use]
    /// Returns the source bytes.
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
}

/// Outcome of an immutable cache write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeParseCacheWrite {
    /// Represents the inserted native parse cache write.
    Inserted,
    /// Represents the already present native parse cache write.
    AlreadyPresent,
}

/// One owned immutable parse-cache row for bounded bulk publication.
pub struct NativeParseCacheEntry {
    key: NativeParseCacheKey,
    payload: Vec<u8>,
}

impl NativeParseCacheEntry {
    /// Bind one exact cache identity to its serialized extraction payload.
    #[must_use]
    pub fn new(key: NativeParseCacheKey, payload: Vec<u8>) -> Self {
        Self { key, payload }
    }
}

/// Insert/no-op counts from one atomic bounded cache publication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeParseCacheBatchWrite {
    /// Newly inserted immutable rows.
    pub inserted: u64,
    /// Exact rows that were already present and only had usage refreshed.
    pub already_present: u64,
}

/// Cross-contract row and byte ceilings for one project's native parse cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeParseCacheRetentionPolicy {
    capacity: NativeParseCacheRetentionCapacity,
    deletion_batch: u32,
}

/// Cross-contract logical capacity retained for one project's native parse cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeParseCacheRetentionCapacity {
    /// Number of contracts.
    pub contracts: u16,
    /// Number of rows.
    pub rows: u64,
    /// Number of bytes used by the payload.
    pub payload_bytes: u64,
}

/// Caller-selected parse-cache ceilings validated as one typed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeParseCacheRetentionPolicyInput {
    /// Capacity for this record.
    pub capacity: NativeParseCacheRetentionCapacity,
    /// Maximum rows removed by one bounded cleanup transaction.
    pub deletion_batch: u32,
}

impl NativeParseCacheRetentionPolicy {
    /// Validate one bounded cache-retention policy.
    /// # Errors
    ///
    /// Returns an error if retained contract, row, payload-byte, or deletion
    /// limits are zero or exceed their cache-retention hard caps.
    pub const fn new(
        input: NativeParseCacheRetentionPolicyInput,
    ) -> Result<Self, NativeParseCacheError> {
        if input.capacity.contracts == 0 || input.capacity.contracts > MAXIMUM_RETAINED_CONTRACTS {
            return Err(NativeParseCacheError::InvalidRetentionPolicy);
        }
        if input.capacity.rows == 0 || input.capacity.rows > MAXIMUM_RETAINED_ROWS {
            return Err(NativeParseCacheError::InvalidRetentionPolicy);
        }
        if input.capacity.payload_bytes == 0
            || input.capacity.payload_bytes > MAXIMUM_RETAINED_PAYLOAD_BYTES
        {
            return Err(NativeParseCacheError::InvalidRetentionPolicy);
        }
        if input.deletion_batch == 0 || input.deletion_batch > MAXIMUM_RETENTION_DELETIONS {
            return Err(NativeParseCacheError::InvalidRetentionPolicy);
        }
        Ok(Self {
            capacity: input.capacity,
            deletion_batch: input.deletion_batch,
        })
    }

    /// Conservative automatic policy: current plus one recent contract, with
    /// independent row, logical-payload-byte, and per-call deletion caps.
    #[must_use]
    pub const fn automatic() -> Self {
        Self {
            capacity: NativeParseCacheRetentionCapacity {
                contracts: DEFAULT_RETAINED_CONTRACTS,
                rows: DEFAULT_RETAINED_ROWS,
                payload_bytes: DEFAULT_RETAINED_PAYLOAD_BYTES,
            },
            deletion_batch: DEFAULT_RETENTION_DELETIONS,
        }
    }

    #[must_use]
    /// Returns the maximum contracts.
    pub const fn maximum_contracts(self) -> u16 {
        self.capacity.contracts
    }

    #[must_use]
    /// Returns the maximum rows.
    pub const fn maximum_rows(self) -> u64 {
        self.capacity.rows
    }

    #[must_use]
    /// Returns the maximum payload bytes.
    pub const fn maximum_payload_bytes(self) -> u64 {
        self.capacity.payload_bytes
    }

    #[must_use]
    /// Returns the maximum deletions.
    pub const fn maximum_deletions(self) -> u32 {
        self.deletion_batch
    }
}

impl Default for NativeParseCacheRetentionPolicy {
    fn default() -> Self {
        Self::automatic()
    }
}

/// Exact logical cache pressure before or after bounded cleanup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeParseCacheStats {
    /// Number of rows.
    pub rows: u64,
    /// Number of contracts.
    pub contracts: u64,
    /// Number of bytes used by the payload.
    pub payload_bytes: u64,
}

/// Bounded cache cleanup result. Physical heap/TOAST reclamation remains a
/// separate explicit database-maintenance concern.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeParseCacheRetentionReport {
    /// Before for this record.
    pub before: NativeParseCacheStats,
    /// After for this record.
    pub after: NativeParseCacheStats,
    /// Number of rows removed.
    pub rows_removed: u64,
    /// Number of payload bytes removed.
    pub payload_bytes_removed: u64,
    /// Whether this value is deletion limit reached.
    pub deletion_limit_reached: bool,
    #[serde(flatten)]
    /// Pressure for this record.
    pub pressure: NativeParseCacheBudgetPressure,
}

/// Remaining logical budget pressure after bounded parse-cache cleanup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeParseCacheBudgetPressure {
    /// Whether this value is over row budget.
    pub over_row_budget: bool,
    /// Whether this value is over payload byte budget.
    pub over_payload_byte_budget: bool,
    /// Whether this value is over contract budget.
    pub over_contract_budget: bool,
}

/// Credential- and source-safe parse-cache failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum NativeParseCacheError {
    #[error("invalid native parse-cache {field}")]
    /// Supplied input violates a documented bound or invariant.
    InvalidInput {
        /// Caller-controlled field that violated its documented bound.
        field: &'static str,
    },
    #[error("native parse-cache row violates its durable contract")]
    /// A stored row violates its durable typed contract.
    CorruptStoredValue,
    #[error("native parse-cache identity produced conflicting immutable facts")]
    /// An existing cache key has different validated payload bytes.
    PayloadConflict,
    #[error("invalid native parse-cache retention policy")]
    /// Retention limits are zero, inconsistent, or exceed hard ceilings.
    InvalidRetentionPolicy,
    #[error("native parse-cache cleanup lost its exact migration lease fence")]
    /// The operation lost its exact project lease fence.
    LeaseFenceLost,
    #[error("Cartograph native storage parse-cache operation failed during {operation}")]
    /// native storage could not complete the named operation.
    DatabaseOperation {
        /// Bounded operation label identifying the failed native storage phase.
        operation: &'static str,
    },
}

fn validate_key(key: &NativeParseCacheKey) -> Result<(), NativeParseCacheError> {
    i64::try_from(key.source_bytes)
        .map(|_| ())
        .map_err(|_| NativeParseCacheError::InvalidInput {
            field: "source_bytes",
        })
}
