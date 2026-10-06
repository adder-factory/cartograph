use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    time::Duration,
};

use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, FileId, GenerationDigestVersion, GenerationId,
    ProjectId, SymbolId, Visibility,
};
use futures_util::TryStreamExt as _;
use serde::Serialize;
use sqlx_core::row::Row;

use crate::{
    CartographDatabase, LeaseFence, StagedGeneration, StorageError,
    database::{audited_query, classify_statement_error, set_local_statement_timeout},
    generation::{
        COPIED_RELATION_PLANNER_COLUMNS, StagingFenceTarget, check_staging_generation_fence,
        lock_staging_generation_fence,
    },
    ingest::{
        CanonicalSearchDocument, CountedCopyRequest, CountedTextCopy, EdgeInput, FileInput,
        GenerationFacts, GenerationValidationError, GenerationValidationLimits,
        LogicalDigestBuilder, NumericalSiteInput, ReferenceInput, SymbolInput, TextRow,
        ValidatedFactTables, canonical_stored_metadata, validate_spill_fact_batch,
    },
    parse_cache::{NativeParseCacheKey, path_digest as parse_cache_path_digest},
};

const MAXIMUM_SPILL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAXIMUM_SPILL_ROWS: u64 = 10_000_000_000;
const MAXIMUM_BATCH_ROWS: usize = 100_000;
const MAXIMUM_BATCH_BYTES: u64 = 72 * 1024 * 1024;
const MAXIMUM_EXTRACTED_BATCH_BYTES: u64 = 257 * 1024 * 1024;
const MAXIMUM_SORT_KEY_BYTES: usize = 128 * 1024;
const MAXIMUM_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
const MAXIMUM_PAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAXIMUM_PAGE_ROWS: i64 = 10_000;
const MAXIMUM_FACT_APPEND_BATCHES: usize = 256;
const MAXIMUM_FACT_APPEND_BYTES: u64 = 256 * 1024 * 1024;
const MAXIMUM_FACT_APPEND_ROWS: u64 = 400_000;
const DEFAULT_SPILL_BYTES: u64 = 128 * 1024 * 1024 * 1024;
const DEFAULT_SPILL_ROWS: u64 = 1_000_000_000;
const CANONICAL_PARTITIONS: u16 = 64;
const BUCKETS_PER_CANONICAL_PARTITION: u16 = 64;
const CANONICAL_PARTITIONS_PER_TRANSACTION: u16 = 4;

/// Explicit PostgreSQL quota for one incomplete native generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeGenerationSpillPolicy {
    maximum_bytes: u64,
    maximum_rows: u64,
}

impl NativeGenerationSpillPolicy {
    /// Validate independent logical-byte and row ceilings.
    /// # Errors
    ///
    /// Returns an error when either ceiling is zero or above the hard spill bound.
    pub const fn new(maximum_bytes: u64, maximum_rows: u64) -> Result<Self, StorageError> {
        if maximum_bytes == 0 || maximum_bytes > MAXIMUM_SPILL_BYTES {
            return Err(StorageError::InvalidInput {
                field: "maximum_spill_bytes",
            });
        }
        if maximum_rows == 0 || maximum_rows > MAXIMUM_SPILL_ROWS {
            return Err(StorageError::InvalidInput {
                field: "maximum_spill_rows",
            });
        }
        Ok(Self {
            maximum_bytes,
            maximum_rows,
        })
    }

    /// Logical sort-key plus payload bytes admitted for one generation.
    #[must_use]
    pub const fn maximum_bytes(self) -> u64 {
        self.maximum_bytes
    }

    /// Raw rows admitted for one generation.
    #[must_use]
    pub const fn maximum_rows(self) -> u64 {
        self.maximum_rows
    }
}

impl Default for NativeGenerationSpillPolicy {
    fn default() -> Self {
        Self {
            maximum_bytes: DEFAULT_SPILL_BYTES,
            maximum_rows: DEFAULT_SPILL_ROWS,
        }
    }
}

/// Durable relation carried by one native spill row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeGenerationSpillRelation {
    /// One serialized file-local extraction result per discovered source file.
    ExtractedFiles,
    /// Unordered file facts.
    Files,
    /// Unordered symbol facts.
    Symbols,
    /// Unordered structural edges.
    Edges,
    /// Unordered reference sites.
    References,
    /// Unordered numerical evidence sites.
    NumericalSites,
    /// Unordered search documents.
    Documents,
}

impl NativeGenerationSpillRelation {
    const FACTS: [Self; 6] = [
        Self::Files,
        Self::Symbols,
        Self::Edges,
        Self::References,
        Self::NumericalSites,
        Self::Documents,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::ExtractedFiles => "extracted_files",
            Self::Files => "files",
            Self::Symbols => "symbols",
            Self::Edges => "edges",
            Self::References => "references",
            Self::NumericalSites => "numerical_sites",
            Self::Documents => "documents",
        }
    }

    /// Raw spill table this relation stages, with the columns its partition
    /// queries filter and join on; `None` for the opaque extraction stream.
    const fn raw_planner_statistics(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::ExtractedFiles => None,
            Self::Files => Some((
                "native_generation_spill_files",
                "project_id, generation_id, bucket, file_id, normalized_path",
            )),
            Self::Symbols => Some((
                "native_generation_spill_symbols",
                "project_id, generation_id, bucket, symbol_id, file_id",
            )),
            Self::Edges => Some((
                "native_generation_spill_edges",
                "project_id, generation_id, bucket, source_symbol_id, target_symbol_id",
            )),
            Self::References => Some((
                "native_generation_spill_references",
                "project_id, generation_id, bucket, file_id, owner_symbol_id, target_symbol_id",
            )),
            Self::NumericalSites => Some((
                "native_generation_spill_numerical_sites",
                "project_id, generation_id, bucket, numerical_site_id, file_id, owner_symbol_id",
            )),
            Self::Documents => Some((
                "native_generation_spill_documents",
                "project_id, generation_id, bucket, document_id, file_id, symbol_id",
            )),
        }
    }

    /// Canonical generation table this relation reduces into, with the planner
    /// columns analyzed once its last partition group lands; `None` for the
    /// opaque extraction stream, which has no canonical table.
    fn canonical_planner_statistics(self) -> Option<(&'static str, &'static str)> {
        let table = match self {
            Self::ExtractedFiles => return None,
            Self::Files => "files",
            Self::Symbols => "symbols",
            Self::Edges => "edges",
            Self::References => "references",
            Self::NumericalSites => "numerical_sites",
            Self::Documents => "search_documents",
        };
        COPIED_RELATION_PLANNER_COLUMNS
            .into_iter()
            .find(|(relation, _)| *relation == table)
    }
}

/// One independently bounded immutable raw spill row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeGenerationSpillRow {
    sort_key: Vec<u8>,
    payload: Vec<u8>,
    payload_digest: ContentDigest,
}

impl NativeGenerationSpillRow {
    /// Validate and digest one opaque payload before database admission.
    /// # Errors
    ///
    /// Returns an error when the key or payload violates its independent row bound.
    pub fn new(sort_key: Vec<u8>, payload: Vec<u8>) -> Result<Self, StorageError> {
        if sort_key.is_empty() || sort_key.len() > MAXIMUM_SORT_KEY_BYTES {
            return Err(StorageError::InvalidInput {
                field: "spill_sort_key",
            });
        }
        if payload.is_empty() || payload.len() > MAXIMUM_PAYLOAD_BYTES {
            return Err(StorageError::InvalidInput {
                field: "spill_payload",
            });
        }
        let payload_digest = ContentDigest::from_bytes(*blake3::hash(&payload).as_bytes());
        Ok(Self {
            sort_key,
            payload,
            payload_digest,
        })
    }

    /// Stable ordering key interpreted by the native reducer.
    #[must_use]
    pub fn sort_key(&self) -> &[u8] {
        &self.sort_key
    }

    /// Exact serialized row bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Digest of the exact serialized row bytes.
    #[must_use]
    pub const fn payload_digest(&self) -> &ContentDigest {
        &self.payload_digest
    }
}

/// One exact parse-cache payload referenced by a spill batch without copying its bytes.
pub struct NativeGenerationSpillCachedRow {
    sort_key: Vec<u8>,
    key: NativeParseCacheKey,
    payload_digest: ContentDigest,
    payload_bytes: u64,
}

/// Immutable parse-cache identity and accounting for one spilled row.
pub struct CachedRowPayload {
    /// Exact parse-cache key the row references.
    pub key: NativeParseCacheKey,
    /// Digest of the referenced immutable payload.
    pub payload_digest: ContentDigest,
    /// Byte size of the referenced payload.
    pub payload_bytes: u64,
}

impl NativeGenerationSpillCachedRow {
    /// Bind one validated sort key to an already verified immutable cache record.
    /// # Errors
    ///
    /// Returns an error when the key or payload size violates spill row bounds.
    pub fn new(sort_key: Vec<u8>, payload: CachedRowPayload) -> Result<Self, StorageError> {
        let CachedRowPayload {
            key,
            payload_digest,
            payload_bytes,
        } = payload;
        if sort_key.is_empty() || sort_key.len() > MAXIMUM_SORT_KEY_BYTES {
            return Err(StorageError::InvalidInput {
                field: "spill_sort_key",
            });
        }
        if payload_bytes == 0 || payload_bytes > usize_to_u64(MAXIMUM_PAYLOAD_BYTES) {
            return Err(StorageError::InvalidInput {
                field: "spill_payload",
            });
        }
        Ok(Self {
            sort_key,
            key,
            payload_digest,
            payload_bytes,
        })
    }
}

/// Physical storage selected for one logically identical extracted-file row.
pub enum NativeGenerationSpillExtractedRow {
    /// Retain the serialized extraction payload directly in the spill relation.
    Inline(NativeGenerationSpillRow),
    /// Reference an immutable parse-cache payload without duplicating its bytes.
    Cached(NativeGenerationSpillCachedRow),
}

impl From<NativeGenerationSpillRow> for NativeGenerationSpillExtractedRow {
    fn from(value: NativeGenerationSpillRow) -> Self {
        Self::Inline(value)
    }
}

impl From<NativeGenerationSpillCachedRow> for NativeGenerationSpillExtractedRow {
    fn from(value: NativeGenerationSpillCachedRow) -> Self {
        Self::Cached(value)
    }
}

impl NativeGenerationSpillExtractedRow {
    fn sort_key(&self) -> &[u8] {
        match self {
            Self::Inline(row) => &row.sort_key,
            Self::Cached(row) => &row.sort_key,
        }
    }

    fn payload_digest(&self) -> &ContentDigest {
        match self {
            Self::Inline(row) => &row.payload_digest,
            Self::Cached(row) => &row.payload_digest,
        }
    }

    fn payload_bytes(&self) -> u64 {
        match self {
            Self::Inline(row) => usize_to_u64(row.payload.len()),
            Self::Cached(row) => row.payload_bytes,
        }
    }

    fn logical_bytes(&self) -> u64 {
        usize_to_u64(self.sort_key().len()).saturating_add(self.payload_bytes())
    }
}

/// Deterministic extracted-file batch whose identity is independent of cache storage.
pub struct NativeGenerationSpillExtractedBatch {
    sequence: u64,
    rows: Vec<NativeGenerationSpillExtractedRow>,
    logical_bytes: u64,
    digest: ContentDigest,
}

struct NativeGenerationSpillRelationPayload {
    relation: NativeGenerationSpillRelation,
    row_count: usize,
    logical_bytes: u64,
    digest: ContentDigest,
}

struct SpillBatchLedgerInput<'a> {
    relation: NativeGenerationSpillRelation,
    sequence: i64,
    rows: usize,
    logical_bytes: u64,
    digest: &'a ContentDigest,
}

#[derive(Clone, Copy)]
struct SpillBatchIdentity<'a> {
    relation: NativeGenerationSpillRelation,
    rows: usize,
    logical_bytes: u64,
    digest: &'a ContentDigest,
}

struct CachedExtractedArrays {
    ordinals: Vec<i32>,
    sort_keys: Vec<Vec<u8>>,
    contracts: Vec<String>,
    path_digests: Vec<String>,
    paths: Vec<String>,
    languages: Vec<String>,
    content_hashes: Vec<String>,
    source_bytes: Vec<i64>,
    payload_digests: Vec<String>,
    payload_bytes: Vec<i64>,
}

struct InlineExtractedArrays {
    ordinals: Vec<i32>,
    sort_keys: Vec<Vec<u8>>,
    payloads: Vec<Vec<u8>>,
    payload_digests: Vec<String>,
}

struct ExtractedRowArrays {
    inline: InlineExtractedArrays,
    cached: CachedExtractedArrays,
}

struct StoredFactPayload {
    row_count: i32,
    logical_bytes: i64,
    digest: String,
}

type StoredFactBatches = BTreeMap<i64, BTreeMap<String, StoredFactPayload>>;

/// Bounded, row-validated unordered facts ready for typed PostgreSQL spill.
pub struct NativeGenerationSpillFactBatch {
    sequence: u64,
    payloads: Vec<NativeGenerationSpillRelationPayload>,
    tables: ValidatedFactTables,
    logical_bytes: u64,
    row_count: u64,
    retained_bytes: u64,
}

/// One sequenced batch of generation facts with the bounds validating it.
pub struct FactBatchInput {
    /// Batch position inside the spilled generation.
    pub sequence: u64,
    /// Facts admitted into this batch.
    pub facts: GenerationFacts,
    /// Exact validation bounds applied to the batch.
    pub limits: GenerationValidationLimits,
}

impl NativeGenerationSpillFactBatch {
    /// Exact logical bytes admitted by this independently validated batch.
    #[must_use]
    pub const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    /// Exact raw rows admitted by this independently validated batch.
    #[must_use]
    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    /// Conservative Rust-owned bytes retained until this batch is copied.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// Validate every row with the in-memory canonical field contract, reduce
    /// duplicates local to this batch, and encode bounded typed relation payloads.
    ///
    /// Cross-relation validation and generation-wide reduction remain deferred
    /// until every spill batch is sealed in PostgreSQL.
    /// # Errors
    ///
    /// Returns an error when validation is cancelled, a field is invalid, or
    /// the batch exceeds the independent row/encoded-byte boundary.
    pub fn new<Cancel>(
        input: FactBatchInput,
        cancelled: Cancel,
    ) -> Result<Self, GenerationValidationError>
    where
        Cancel: FnMut() -> bool,
    {
        let FactBatchInput {
            sequence,
            facts,
            limits,
        } = input;
        if sequence > i64::MAX.unsigned_abs() {
            return Err(StorageError::InvalidInput {
                field: "spill_batch_sequence",
            }
            .into());
        }
        let (tables, retained_bytes) = validate_spill_fact_batch(facts, limits, cancelled)?;
        let payloads = [
            encode_relation_payload(&tables.files)?,
            encode_relation_payload(&tables.symbols)?,
            encode_relation_payload(&tables.edges)?,
            encode_relation_payload(&tables.references)?,
            encode_relation_payload(&tables.numerical_sites)?,
            encode_relation_payload(&tables.documents)?,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if payloads.is_empty() {
            return Err(StorageError::InvalidInput {
                field: "spill_fact_batch_rows",
            }
            .into());
        }
        let (logical_bytes, row_count) = payloads
            .iter()
            .try_fold((0_u64, 0_u64), |(bytes, rows), payload| {
                Some((
                    bytes.checked_add(payload.logical_bytes)?,
                    rows.checked_add(usize_to_u64(payload.row_count))?,
                ))
            })
            .filter(|(bytes, rows)| {
                *bytes > 0
                    && *bytes <= MAXIMUM_BATCH_BYTES
                    && *rows > 0
                    && *rows <= usize_to_u64(MAXIMUM_BATCH_ROWS)
            })
            .ok_or(StorageError::InvalidInput {
                field: "spill_fact_batch_bytes",
            })?;
        Ok(Self {
            sequence,
            payloads,
            tables,
            logical_bytes,
            row_count,
            retained_bytes,
        })
    }

    /// Unordered rows retained by this locally reduced batch.
    #[must_use]
    pub fn counts(&self) -> NativeGenerationSpillFactCounts {
        let mut counts = NativeGenerationSpillFactCounts::default();
        for payload in &self.payloads {
            let rows = usize_to_u64(payload.row_count);
            match payload.relation {
                NativeGenerationSpillRelation::ExtractedFiles => {}
                NativeGenerationSpillRelation::Files => counts.files = rows,
                NativeGenerationSpillRelation::Symbols => counts.symbols = rows,
                NativeGenerationSpillRelation::Edges => counts.edges = rows,
                NativeGenerationSpillRelation::References => counts.references = rows,
                NativeGenerationSpillRelation::NumericalSites => counts.numerical_sites = rows,
                NativeGenerationSpillRelation::Documents => counts.documents = rows,
            }
        }
        counts
    }
}

fn validate_extracted_batch_bounds(sequence: u64, rows: usize) -> Result<(), StorageError> {
    if sequence > i64::MAX.unsigned_abs() {
        return Err(StorageError::InvalidInput {
            field: "spill_batch_sequence",
        });
    }
    if rows == 0 || rows > MAXIMUM_BATCH_ROWS {
        return Err(StorageError::InvalidInput {
            field: "spill_batch_rows",
        });
    }
    let final_offset =
        u64::try_from(rows.saturating_sub(1)).map_err(|_| StorageError::InvalidInput {
            field: "spill_batch_rows",
        })?;
    if sequence
        .checked_add(final_offset)
        .is_none_or(|final_sequence| final_sequence > i64::MAX.unsigned_abs())
    {
        return Err(StorageError::InvalidInput {
            field: "spill_batch_sequence",
        });
    }
    Ok(())
}

impl NativeGenerationSpillExtractedBatch {
    /// Validate and digest one contiguous logical extraction batch.
    ///
    /// The digest deliberately uses only sort key, payload length, and payload
    /// digest. Inline bytes and immutable parse-cache references therefore replay
    /// under one identity and cannot change deterministic batch boundaries.
    /// # Errors
    ///
    /// Returns an error when the sequence, row count, or encoded bytes exceed a hard bound.
    pub fn new(
        sequence: u64,
        rows: Vec<NativeGenerationSpillExtractedRow>,
    ) -> Result<Self, StorageError> {
        validate_extracted_batch_bounds(sequence, rows.len())?;
        let logical_bytes = rows
            .iter()
            .try_fold(0_u64, |total, row| total.checked_add(row.logical_bytes()))
            .filter(|bytes| *bytes > 0 && *bytes <= MAXIMUM_EXTRACTED_BATCH_BYTES)
            .ok_or(StorageError::InvalidInput {
                field: "spill_batch_bytes",
            })?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cartograph-v2-native-generation-spill-extracted-batch-v1");
        hasher.update(&sequence.to_be_bytes());
        hasher.update(&usize_to_u64(rows.len()).to_be_bytes());
        for row in &rows {
            hasher.update(&usize_to_u64(row.sort_key().len()).to_be_bytes());
            hasher.update(row.sort_key());
            hasher.update(&row.payload_bytes().to_be_bytes());
            hasher.update(row.payload_digest().as_str().as_bytes());
        }
        Ok(Self {
            sequence,
            rows,
            logical_bytes,
            digest: ContentDigest::from_bytes(*hasher.finalize().as_bytes()),
        })
    }
}

/// Idempotent append result for one deterministic spill batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeGenerationSpillWrite {
    /// The batch committed in this call.
    Inserted,
    /// An exact byte-identical batch was already durable.
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeGenerationSpillPhase {
    Parsing,
    Resolving,
    Sealed,
    Canonicalizing,
    Canonicalized,
}

/// Durable restart point for one generation-fenced native spill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeGenerationSpillState {
    /// File-local extraction batches are still being admitted.
    Parsing,
    /// Extraction is complete and resolved fact batches are being admitted.
    Resolving,
    /// Every resolved fact batch and its exact counts are sealed.
    Sealed,
    /// One or more deterministic canonical partitions have committed.
    Canonicalizing,
    /// Every typed relation is canonical and ready for streamed digest verification.
    Canonicalized,
}

impl From<NativeGenerationSpillPhase> for NativeGenerationSpillState {
    fn from(value: NativeGenerationSpillPhase) -> Self {
        match value {
            NativeGenerationSpillPhase::Parsing => Self::Parsing,
            NativeGenerationSpillPhase::Resolving => Self::Resolving,
            NativeGenerationSpillPhase::Sealed => Self::Sealed,
            NativeGenerationSpillPhase::Canonicalizing => Self::Canonicalizing,
            NativeGenerationSpillPhase::Canonicalized => Self::Canonicalized,
        }
    }
}

impl NativeGenerationSpillPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Parsing => "parsing",
            Self::Resolving => "resolving",
            Self::Sealed => "sealed",
            Self::Canonicalizing => "canonicalizing",
            Self::Canonicalized => "canonicalized",
        }
    }

    fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw {
            "parsing" => Ok(Self::Parsing),
            "resolving" => Ok(Self::Resolving),
            "sealed" => Ok(Self::Sealed),
            "canonicalizing" => Ok(Self::Canonicalizing),
            "canonicalized" => Ok(Self::Canonicalized),
            _ => Err(StorageError::CorruptStoredValue {
                field: "native_generation_spill_phase",
            }),
        }
    }
}

/// Fixed-size durable accounting for one native generation spill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeGenerationSpillReport {
    /// Durable restart point observed under the exact generation fence.
    pub state: NativeGenerationSpillState,
    /// Sort-key plus serialized payload bytes written so far.
    pub logical_bytes: u64,
    /// Raw rows written so far.
    pub raw_rows: u64,
    /// File-local extraction payloads durably retained.
    pub extracted_files: u64,
    /// Configured logical-byte quota.
    pub maximum_bytes: u64,
    /// Configured raw-row quota.
    pub maximum_rows: u64,
}

/// Exact unordered fact counts used to seal a completed resolver spill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeGenerationSpillFactCounts {
    /// File rows before generation-wide deduplication.
    pub files: u64,
    /// Symbol rows before generation-wide deduplication.
    pub symbols: u64,
    /// Edge rows before generation-wide aggregation.
    pub edges: u64,
    /// Reference rows before generation-wide deduplication.
    pub references: u64,
    /// Numerical site rows before generation-wide deduplication.
    pub numerical_sites: u64,
    /// Search document rows before generation-wide deduplication.
    pub documents: u64,
}

/// One generation-local centrality patch for an unordered spilled symbol row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeGenerationSpillCentralityScore {
    symbol_id: SymbolId,
    betweenness_ppb: Option<u32>,
    pagerank_ppb: Option<u32>,
}

impl NativeGenerationSpillCentralityScore {
    /// Bind optional derived scores to one stable symbol identity.
    /// # Errors
    ///
    /// Returns an error when neither centrality calculation was enabled or a score
    /// exceeds the normalized parts-per-billion range.
    pub fn new(
        symbol_id: SymbolId,
        betweenness_ppb: Option<u32>,
        pagerank_ppb: Option<u32>,
    ) -> Result<Self, StorageError> {
        if betweenness_ppb.is_none() && pagerank_ppb.is_none() {
            return Err(StorageError::InvalidInput {
                field: "spill_centrality_scores",
            });
        }
        if matches!(betweenness_ppb, Some(score) if score > 1_000_000_000)
            || matches!(pagerank_ppb, Some(score) if score > 1_000_000_000)
        {
            return Err(StorageError::InvalidInput {
                field: "spill_centrality_score",
            });
        }
        Ok(Self {
            symbol_id,
            betweenness_ppb,
            pagerank_ppb,
        })
    }

    /// Stable identity receiving the derived scores.
    #[must_use]
    pub const fn symbol_id(&self) -> &SymbolId {
        &self.symbol_id
    }
}

impl NativeGenerationSpillFactCounts {
    /// Checked total across every typed fact relation.
    /// # Errors
    ///
    /// Returns an error if the count cannot be represented by the spill contract.
    pub fn total(self) -> Result<u64, StorageError> {
        [
            self.files,
            self.symbols,
            self.edges,
            self.references,
            self.numerical_sites,
            self.documents,
        ]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
        .ok_or(StorageError::GenerationSpillLimitReached { resource: "rows" })
    }
}

/// One committed database-reduction partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeGenerationSpillCanonicalProgress {
    /// Relation reduced by this call.
    pub relation: NativeGenerationSpillRelation,
    /// Zero-based deterministic hash partition.
    pub partition: u16,
    /// Canonical rows inserted into the immutable generation tables.
    pub canonical_rows: u64,
    /// Whether every typed relation is now canonicalized.
    pub complete: bool,
}

struct NativeGenerationCanonicalCursor {
    relation: NativeGenerationSpillRelation,
    partition: u16,
    lower_bucket: i32,
    upper_bucket: i32,
}

impl NativeGenerationCanonicalCursor {
    /// The raw spill table to analyze before this group reduces anything: the
    /// relation's first group is the first to read it, and every raw row of
    /// this generation was sealed before canonicalization began.
    const fn raw_statistics(&self) -> Option<(&'static str, &'static str)> {
        if self.partition == 0 {
            self.relation.raw_planner_statistics()
        } else {
            None
        }
    }

    /// The canonical table to analyze after this group, when it is the
    /// relation's last, so later relations validate against all of it.
    fn canonical_statistics(&self) -> Option<(&'static str, &'static str)> {
        let completes_relation = self
            .partition
            .saturating_add(CANONICAL_PARTITIONS_PER_TRANSACTION)
            >= CANONICAL_PARTITIONS;
        completes_relation
            .then(|| self.relation.canonical_planner_statistics())
            .flatten()
    }
}

/// Deterministic digest and counts streamed from canonical PostgreSQL rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeGenerationSpillDigestReport {
    project_id: ProjectId,
    generation_id: GenerationId,
    /// Logical generation digest under the current versioned contract.
    digest: ContentDigest,
    /// Canonical relation counts included by the digest.
    counts: NativeGenerationSpillFactCounts,
}

impl NativeGenerationSpillDigestReport {
    /// Logical generation digest under the current versioned contract.
    #[must_use]
    pub const fn digest(&self) -> &ContentDigest {
        &self.digest
    }

    /// Canonical relation counts included by the digest.
    #[must_use]
    pub const fn counts(&self) -> NativeGenerationSpillFactCounts {
        self.counts
    }

    pub(crate) fn matches(&self, project_id: &ProjectId, generation_id: &GenerationId) -> bool {
        &self.project_id == project_id && &self.generation_id == generation_id
    }
}

/// Cursor for ordered file-local extraction reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeGenerationExtractedCursor {
    sequence: Option<u64>,
}

impl NativeGenerationExtractedCursor {
    /// Last durable parse sequence included by a page.
    #[must_use]
    pub const fn sequence(self) -> Option<u64> {
        self.sequence
    }
}

/// Bounded ordered file-local extraction page.
#[derive(Debug)]
pub struct NativeGenerationExtractedPage {
    rows: Vec<(u64, NativeGenerationSpillRow)>,
    next: NativeGenerationExtractedCursor,
}

struct ExtractedPageWindow {
    end_sequence: u64,
    row_count: usize,
}

impl NativeGenerationExtractedPage {
    /// Ordered immutable rows. The sequence is the original parse-stage order.
    #[must_use]
    pub fn rows(&self) -> &[(u64, NativeGenerationSpillRow)] {
        &self.rows
    }

    /// Cursor for the next page.
    #[must_use]
    pub const fn next(&self) -> NativeGenerationExtractedCursor {
        self.next
    }

    /// Whether the ordered scan reached the current end.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Relation and batches for one typed spilled-fact insert.
struct TypedFactPayloads<'payloads> {
    relation: NativeGenerationSpillRelation,
    batches: &'payloads [(i64, &'payloads NativeGenerationSpillFactBatch)],
}

/// Bounded totals one spill write adds to the run's running accounting.
#[derive(Clone, Copy)]
struct SpillTotalsDelta {
    logical_bytes: u64,
    rows: u64,
    extracted_files: u64,
}

/// Exact paging position inside one spilled extraction stream.
#[derive(Clone, Copy)]
struct ExtractedPageCursor {
    after: i64,
    minimum_batch_sequence: i64,
}

/// Lease fence, spill policy, and deadline bound to one spilled generation.
pub struct NativeGenerationSpillRequest {
    /// Exact lease fence the spill must hold for its whole lifetime.
    pub fence: LeaseFence,
    /// Byte and row ceilings admitted for this spill.
    pub policy: NativeGenerationSpillPolicy,
    /// Per-statement deadline applied to every spill statement.
    pub statement_timeout: Duration,
}

/// Exact generation and lease-fenced PostgreSQL spill authority.
#[derive(Clone)]
pub struct NativeGenerationSpill {
    database: CartographDatabase,
    project_id: ProjectId,
    generation_id: GenerationId,
    generation_sequence: i64,
    fence: LeaseFence,
    policy: NativeGenerationSpillPolicy,
    statement_timeout: Duration,
}

impl fmt::Debug for NativeGenerationSpill {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeGenerationSpill")
            .field("policy", &self.policy)
            .field("statement_timeout", &self.statement_timeout)
            .finish_non_exhaustive()
    }
}

impl NativeGenerationSpill {
    /// Bind spill-only authority to an exact staging token and live lease fence.
    ///
    /// This capability cannot publish or terminalize a generation. Every method
    /// rechecks the exact database-clock lease and staging state before mutation.
    /// # Errors
    ///
    /// Returns an error when the timeout is zero or the fence targets another generation.
    pub fn new(
        database: CartographDatabase,
        generation: &StagedGeneration,
        request: NativeGenerationSpillRequest,
    ) -> Result<Self, StorageError> {
        let NativeGenerationSpillRequest {
            fence,
            policy,
            statement_timeout,
        } = request;
        if statement_timeout.is_zero()
            || fence.target().project_id() != generation.project_id()
            || fence.target().generation_id() != Some(generation.generation_id())
        {
            return Err(StorageError::LeaseFenceLost);
        }
        Ok(Self {
            database,
            project_id: generation.project_id().clone(),
            generation_id: generation.generation_id().clone(),
            generation_sequence: generation.sequence(),
            fence,
            policy,
            statement_timeout,
        })
    }

    /// Create or verify the exact durable spill run under the live generation fence.
    /// # Errors
    ///
    /// Returns an error when the generation/lease changed, policy conflicts with an
    /// existing run, or the bounded transaction cannot commit.
    pub async fn initialize(&self) -> Result<NativeGenerationSpillReport, StorageError> {
        let schema = crate::database::quoted_schema(&self.database.schema);
        let mut transaction = begin_spill_transaction(self, "spill-initialize-begin").await?;
        let insert = format!(
            r#"INSERT INTO {schema}."native_generation_spills" (
                    project_id, generation_id, generation_sequence,
                    maximum_bytes, maximum_rows
                ) VALUES (CAST($1 AS uuid), CAST($2 AS uuid), $3, $4, $5)
                ON CONFLICT (project_id, generation_id) DO NOTHING"#
        );
        audited_query(insert)
            .bind(self.project_id.as_str())
            .bind(self.generation_id.as_str())
            .bind(self.generation_sequence)
            .bind(as_i64(self.policy.maximum_bytes, "maximum_spill_bytes")?)
            .bind(as_i64(self.policy.maximum_rows, "maximum_spill_rows")?)
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("spill-initialize"))?;
        let (_, report) = load_run_for_update(self, &mut transaction).await?;
        if report.maximum_bytes != self.policy.maximum_bytes
            || report.maximum_rows != self.policy.maximum_rows
        {
            return Err(StorageError::GenerationSpillConflict);
        }
        commit_transaction(self, transaction, "spill-initialize-commit").await?;
        Ok(report)
    }

    /// Read fixed-size logical quota and admitted-row accounting for this exact run.
    /// # Errors
    ///
    /// Returns an error when the generation fence is no longer live or the run is absent.
    pub async fn report(&self) -> Result<NativeGenerationSpillReport, StorageError> {
        let mut transaction = begin_spill_transaction(self, "spill-report-begin").await?;
        let (_, report) = load_run_for_update(self, &mut transaction).await?;
        commit_transaction(self, transaction, "spill-report-commit").await?;
        Ok(report)
    }

    /// Append one deterministic extracted-file batch under its exact fence and quota.
    ///
    /// Inline and parse-cache-backed rows may coexist in one logical batch. Its
    /// ledger identity and contiguous sequence window are independent of that
    /// physical representation, so cache failures cannot make retries conflict.
    /// # Errors
    ///
    /// Returns an error when a cache identity is foreign or absent, the phase/fence
    /// changed, a retry conflicts, quota is exhausted, or the transaction cannot commit.
    pub async fn append_extracted_batch(
        &self,
        batch: NativeGenerationSpillExtractedBatch,
    ) -> Result<NativeGenerationSpillWrite, StorageError> {
        if batch.rows.iter().any(|row| {
            matches!(
                row,
                NativeGenerationSpillExtractedRow::Cached(cached)
                    if cached.key.project_id() != &self.project_id
            )
        }) {
            return Err(StorageError::InvalidInput {
                field: "spill_cache_project_id",
            });
        }
        let mut transaction = begin_spill_transaction(self, "spill-append-begin").await?;
        let (phase, report) = load_run_for_update(self, &mut transaction).await?;
        let sequence = as_i64(batch.sequence, "spill_batch_sequence")?;
        if opaque_batch_exists(
            &mut SpillWrite {
                spill: self,
                transaction: &mut transaction,
            },
            sequence,
            SpillBatchIdentity {
                relation: NativeGenerationSpillRelation::ExtractedFiles,
                rows: batch.rows.len(),
                logical_bytes: batch.logical_bytes,
                digest: &batch.digest,
            },
        )
        .await?
        {
            commit_transaction(self, transaction, "spill-read-batch-commit").await?;
            return Ok(NativeGenerationSpillWrite::AlreadyPresent);
        }
        if phase != NativeGenerationSpillPhase::Parsing {
            return Err(StorageError::InvalidGenerationTransition {
                actual: phase.as_str().to_owned(),
                requested: NativeGenerationSpillPhase::Parsing.as_str(),
            });
        }
        write_new_extracted_batch(
            &mut SpillWrite {
                spill: self,
                transaction: &mut transaction,
            },
            NewExtractedBatch {
                report,
                sequence,
                batch: &batch,
            },
        )
        .await?;
        commit_transaction(self, transaction, "spill-append-commit").await?;
        Ok(NativeGenerationSpillWrite::Inserted)
    }

    /// Append one row-validated unordered fact batch to typed spill relations.
    ///
    /// All non-empty relations and their retry metadata commit in one fenced
    /// transaction. An exact retry is a no-op; partial or byte-different reuse of
    /// the sequence fails closed.
    /// # Errors
    ///
    /// Returns an error when the run is not resolving, the fence changed, a retry
    /// conflicts, quota is exhausted, or PostgreSQL cannot commit every relation.
    pub async fn append_fact_batch(
        &self,
        batch: NativeGenerationSpillFactBatch,
    ) -> Result<NativeGenerationSpillWrite, StorageError> {
        let mut writes = self.append_fact_batches(vec![batch]).await?;
        writes
            .pop()
            .ok_or_else(|| database_error("spill-fact-write"))
    }

    /// Append a bounded group of independently validated unordered fact batches.
    ///
    /// Existing exact batches remain no-ops, while all new ledgers and typed rows
    /// share one fenced PostgreSQL transaction. This preserves each batch's retry
    /// identity without paying one transaction and one round trip per source file.
    /// # Errors
    ///
    /// Returns an error when the group bound, run phase, fence, retry identity,
    /// quota, typed-row count, or complete transaction cannot be proven.
    pub async fn append_fact_batches(
        &self,
        batches: Vec<NativeGenerationSpillFactBatch>,
    ) -> Result<Vec<NativeGenerationSpillWrite>, StorageError> {
        validate_fact_append_group(&batches)?;
        let mut transaction = begin_spill_transaction(self, "spill-append-facts-begin").await?;
        let (phase, report) = load_run_for_update(self, &mut transaction).await?;
        let sequences = batches
            .iter()
            .map(|batch| as_i64(batch.sequence, "spill_batch_sequence"))
            .collect::<Result<Vec<_>, _>>()?;
        let existing = existing_fact_batches(self, &mut transaction, &sequences).await?;
        let admission = admit_fact_batches(
            FactAdmissionInput {
                phase,
                report,
                existing: &existing,
            },
            sequences,
            &batches,
        )?;
        insert_admitted_fact_batches(
            &mut SpillWrite {
                spill: self,
                transaction: &mut transaction,
            },
            &admission,
        )
        .await?;
        commit_transaction(self, transaction, "spill-append-facts-commit").await?;
        Ok(admission.writes)
    }

    /// Apply one bounded, identity-ordered centrality patch before resolution is sealed.
    ///
    /// Every requested identity must update exactly one unordered symbol row. This rejects
    /// missing and duplicate identities before canonical reduction can hide either condition.
    /// # Errors
    ///
    /// Returns an error when the batch is empty/oversized/not strictly ordered, the spill is
    /// outside its resolving phase, the exact fence was lost, or an identity is not unique.
    pub async fn apply_centrality_scores(
        &self,
        scores: &[NativeGenerationSpillCentralityScore],
    ) -> Result<(), StorageError> {
        if scores.is_empty() || scores.len() > MAXIMUM_BATCH_ROWS {
            return Err(StorageError::InvalidInput {
                field: "spill_centrality_scores",
            });
        }
        if !scores
            .is_sorted_by(|previous, next| previous.symbol_id.as_str() < next.symbol_id.as_str())
        {
            return Err(StorageError::InvalidInput {
                field: "spill_centrality_order",
            });
        }
        let schema = crate::database::quoted_schema(&self.database.schema);
        let mut transaction = begin_spill_transaction(self, "spill-centrality-begin").await?;
        let (phase, _) = load_run_for_update(self, &mut transaction).await?;
        if phase != NativeGenerationSpillPhase::Resolving {
            return Err(StorageError::InvalidGenerationTransition {
                actual: phase.as_str().to_owned(),
                requested: NativeGenerationSpillPhase::Resolving.as_str(),
            });
        }
        let symbol_ids = scores
            .iter()
            .map(|score| score.symbol_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let betweenness = scores
            .iter()
            .map(|score| {
                score
                    .betweenness_ppb
                    .map(|value| f64::from(value) / 1_000_000_000.0)
            })
            .collect::<Vec<_>>();
        let pagerank = scores
            .iter()
            .map(|score| {
                score
                    .pagerank_ppb
                    .map(|value| f64::from(value) / 1_000_000_000.0)
            })
            .collect::<Vec<_>>();
        let update = format!(
            r#"UPDATE {schema}."native_generation_spill_symbols" AS symbols
                SET betweenness = input.betweenness,
                    pagerank = input.pagerank
                FROM unnest(
                    CAST($3 AS uuid[]), CAST($4 AS double precision[]),
                    CAST($5 AS double precision[])
                ) AS input(symbol_id, betweenness, pagerank)
                WHERE symbols.project_id = CAST($1 AS uuid)
                  AND symbols.generation_id = CAST($2 AS uuid)
                  AND symbols.symbol_id = input.symbol_id"#
        );
        let updated = audited_query(update)
            .bind(self.project_id.as_str())
            .bind(self.generation_id.as_str())
            .bind(symbol_ids)
            .bind(betweenness)
            .bind(pagerank)
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("spill-centrality-update"))?;
        if updated.rows_affected() != usize_to_u64(scores.len()) {
            return Err(StorageError::GenerationSpillConflict);
        }
        commit_transaction(self, transaction, "spill-centrality-commit").await?;
        Ok(())
    }

    /// Seal complete unordered resolver output before generation-wide reduction.
    /// # Errors
    ///
    /// Returns an error when any typed relation count differs from the caller's
    /// deterministic accounting, the phase/fence changed, or the transition fails.
    pub async fn seal_resolution(
        &self,
        expected: NativeGenerationSpillFactCounts,
    ) -> Result<(), StorageError> {
        let _ = expected.total()?;
        let schema = crate::database::quoted_schema(&self.database.schema);
        let mut transaction = begin_spill_transaction(self, "spill-seal-resolution-begin").await?;
        let (phase, _) = load_run_for_update(self, &mut transaction).await?;
        if matches!(
            phase,
            NativeGenerationSpillPhase::Sealed
                | NativeGenerationSpillPhase::Canonicalizing
                | NativeGenerationSpillPhase::Canonicalized
        ) {
            commit_transaction(self, transaction, "spill-seal-resolution-commit").await?;
            return Ok(());
        }
        if phase != NativeGenerationSpillPhase::Resolving {
            return Err(StorageError::InvalidGenerationTransition {
                actual: phase.as_str().to_owned(),
                requested: NativeGenerationSpillPhase::Sealed.as_str(),
            });
        }
        let counts_sql = fact_count_sql(&schema, "native_generation_spill_");
        let row = audited_query(counts_sql)
            .bind(self.project_id.as_str())
            .bind(self.generation_id.as_str())
            .fetch_one(&mut *transaction)
            .await
            .map_err(|_| database_error("spill-count-resolution"))?;
        let actual = decode_fact_counts(&row)?;
        if actual != expected {
            return Err(StorageError::GenerationSpillConflict);
        }
        let update = format!(
            r#"UPDATE {schema}."native_generation_spills"
                SET phase = 'sealed', canonical_relation = 'files',
                    canonical_partition = 0, updated_at = clock_timestamp()
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND phase = 'resolving'"#
        );
        let updated = audited_query(update)
            .bind(self.project_id.as_str())
            .bind(self.generation_id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("spill-seal-resolution"))?;
        if updated.rows_affected() != 1 {
            return Err(StorageError::GenerationSpillConflict);
        }
        commit_transaction(self, transaction, "spill-seal-resolution-commit").await?;
        Ok(())
    }

    /// Reduce one bounded contiguous hash-partition group into immutable generation tables.
    ///
    /// PostgreSQL performs generation-wide grouping and can spill its bounded sort/hash
    /// work to database temporary storage. The completed raw group is removed in
    /// the same transaction as the canonical insert and durable progress advance.
    /// A relation's first group refreshes the planner statistics of its raw spill
    /// table before reading it, and its last group refreshes those of the
    /// canonical table it loaded before committing, because every later
    /// relation validates its rows against this generation's files and symbols.
    /// # Errors
    ///
    /// Returns an error when duplicate identities conflict, aggregation overflows,
    /// the phase/fence changed, or the partition transaction cannot commit. A
    /// statement that exceeds the spill statement timeout is reported as
    /// [`StorageError::StatementTimeout`].
    pub async fn canonicalize_next(
        &self,
    ) -> Result<NativeGenerationSpillCanonicalProgress, StorageError> {
        let mut transaction = begin_spill_transaction(self, "spill-canonicalize-begin").await?;
        let (phase, _) = load_run_for_update(self, &mut transaction).await?;
        if phase == NativeGenerationSpillPhase::Canonicalized {
            commit_transaction(self, transaction, "spill-canonicalize-complete-commit").await?;
            return Ok(NativeGenerationSpillCanonicalProgress {
                relation: NativeGenerationSpillRelation::Documents,
                partition: CANONICAL_PARTITIONS.saturating_sub(1),
                canonical_rows: 0,
                complete: true,
            });
        }
        begin_canonicalization(self, &mut transaction, phase).await?;
        let cursor = load_canonical_cursor(self, &mut transaction).await?;
        analyze_planner_statistics(self, &mut transaction, cursor.raw_statistics()).await?;
        let inserted = reduce_canonical_partition(self, &mut transaction, &cursor).await?;
        analyze_planner_statistics(self, &mut transaction, cursor.canonical_statistics()).await?;
        let complete = advance_canonical_cursor(
            self,
            &mut transaction,
            ReducedPartition {
                cursor: &cursor,
                inserted,
            },
        )
        .await?;
        commit_transaction(self, transaction, "spill-canonicalize-commit").await?;
        Ok(NativeGenerationSpillCanonicalProgress {
            relation: cursor.relation,
            partition: cursor.partition,
            canonical_rows: inserted,
            complete,
        })
    }

    /// Stream the complete canonical generation in the same table/key order used
    /// by the in-memory digest contract.
    ///
    /// The callback receives bounded row increments and may update a supervisor
    /// watchdog without receiving source or identifier data.
    /// # Errors
    ///
    /// Returns an error when cancellation is observed, the phase/fence changed,
    /// canonical rows cannot be decoded, or the repeatable traversal fails.
    pub async fn compute_digest<Cancel, Progress, ProgressFuture>(
        &self,
        mut cancelled: Cancel,
        mut progress: Progress,
    ) -> Result<NativeGenerationSpillDigestReport, StorageError>
    where
        Cancel: FnMut() -> bool,
        Progress: FnMut(u64) -> ProgressFuture,
        ProgressFuture: Future<Output = bool>,
    {
        let schema = crate::database::quoted_schema(&self.database.schema);
        let mut transaction = begin_spill_transaction(self, "spill-digest-begin").await?;
        let (phase, _) = load_run_for_update(self, &mut transaction).await?;
        if phase != NativeGenerationSpillPhase::Canonicalized {
            return Err(StorageError::InvalidGenerationTransition {
                actual: phase.as_str().to_owned(),
                requested: NativeGenerationSpillPhase::Canonicalized.as_str(),
            });
        }
        let (digest, counts) = stream_spilled_digest(
            &mut transaction,
            SpillScope {
                schema: &schema,
                project_id: &self.project_id,
                generation_id: &self.generation_id,
            },
            DigestObservation {
                cancelled: &mut cancelled,
                progress: &mut progress,
            },
        )
        .await?;
        if cancelled() {
            return Err(StorageError::GenerationSpillCancelled);
        }
        commit_transaction(self, transaction, "spill-digest-commit").await?;
        Ok(NativeGenerationSpillDigestReport {
            project_id: self.project_id.clone(),
            generation_id: self.generation_id.clone(),
            digest,
            counts,
        })
    }

    /// Seal the parse relation and admit only unordered resolved-fact batches.
    /// # Errors
    ///
    /// Returns an error when the exact expected file count is not durable, the phase
    /// changed, the fence was lost, or the bounded transition cannot commit.
    pub async fn finish_parsing(&self, expected_files: u64) -> Result<(), StorageError> {
        let schema = crate::database::quoted_schema(&self.database.schema);
        let mut transaction = begin_spill_transaction(self, "spill-finish-parsing-begin").await?;
        let (phase, report) = load_run_for_update(self, &mut transaction).await?;
        if matches!(
            phase,
            NativeGenerationSpillPhase::Resolving
                | NativeGenerationSpillPhase::Sealed
                | NativeGenerationSpillPhase::Canonicalizing
                | NativeGenerationSpillPhase::Canonicalized
        ) && report.extracted_files == expected_files
        {
            commit_transaction(self, transaction, "spill-finish-parsing-commit").await?;
            return Ok(());
        }
        if phase != NativeGenerationSpillPhase::Parsing || report.extracted_files != expected_files
        {
            return Err(StorageError::GenerationSpillConflict);
        }
        let update = format!(
            r#"UPDATE {schema}."native_generation_spills"
                SET phase = 'resolving', updated_at = clock_timestamp()
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND phase = 'parsing'"#
        );
        let result = audited_query(update)
            .bind(self.project_id.as_str())
            .bind(self.generation_id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("spill-finish-parsing"))?;
        if result.rows_affected() != 1 {
            return Err(StorageError::GenerationSpillConflict);
        }
        commit_transaction(self, transaction, "spill-finish-parsing-commit").await?;
        Ok(())
    }

    /// Read a byte-bounded ordered page of serialized file-local extraction results.
    ///
    /// One independently bounded row is indivisible and may exceed `maximum_bytes`.
    /// # Errors
    ///
    /// Returns an error when the page bound is invalid, the phase/fence changed, or
    /// stored payload identity cannot be verified.
    pub async fn load_extracted_page(
        &self,
        cursor: NativeGenerationExtractedCursor,
        maximum_bytes: u64,
    ) -> Result<NativeGenerationExtractedPage, StorageError> {
        if maximum_bytes == 0 || maximum_bytes > MAXIMUM_PAGE_BYTES {
            return Err(StorageError::InvalidInput {
                field: "spill_page_bytes",
            });
        }
        let mut transaction = begin_spill_transaction(self, "spill-read-page-begin").await?;
        let (phase, _) = load_run_for_update(self, &mut transaction).await?;
        if !matches!(
            phase,
            NativeGenerationSpillPhase::Resolving
                | NativeGenerationSpillPhase::Sealed
                | NativeGenerationSpillPhase::Canonicalizing
                | NativeGenerationSpillPhase::Canonicalized
        ) {
            return Err(StorageError::InvalidGenerationTransition {
                actual: phase.as_str().to_owned(),
                requested: NativeGenerationSpillPhase::Resolving.as_str(),
            });
        }
        let after = cursor
            .sequence
            .map(|sequence| as_i64(sequence, "spill_page_cursor"))
            .transpose()?
            .unwrap_or(-1);
        let maximum_row_offset = i64::try_from(MAXIMUM_BATCH_ROWS.saturating_sub(1))
            .map_err(|_| database_error("spill-page-row-offset"))?;
        let minimum_batch_sequence = after.saturating_sub(maximum_row_offset).max(0);
        let window = select_extracted_page_window(
            &mut SpillWrite {
                spill: self,
                transaction: &mut transaction,
            },
            ExtractedPageCursor {
                after,
                minimum_batch_sequence,
            },
            maximum_bytes,
        )
        .await?;
        let Some(window) = window else {
            commit_transaction(self, transaction, "spill-read-page-commit").await?;
            return Ok(NativeGenerationExtractedPage {
                rows: Vec::new(),
                next: cursor,
            });
        };
        let decoded = load_extracted_page_payloads(
            &mut SpillWrite {
                spill: self,
                transaction: &mut transaction,
            },
            ExtractedPageCursor {
                after,
                minimum_batch_sequence,
            },
            &window,
        )
        .await?;
        let next = NativeGenerationExtractedCursor {
            sequence: decoded.last().map(|(sequence, _)| *sequence),
        };
        commit_transaction(self, transaction, "spill-read-page-commit").await?;
        Ok(NativeGenerationExtractedPage {
            rows: decoded,
            next,
        })
    }
}

/// Bounded PostgreSQL row admission and paging for one spilled generation.
///
/// These are the storage steps behind the public spill lifecycle; keeping them
/// beside the type rather than on it keeps the type's own surface to the
/// lifecycle callers actually use.
async fn opaque_batch_exists(
    write: &mut SpillWrite<'_, '_>,
    sequence: i64,
    identity: SpillBatchIdentity<'_>,
) -> Result<bool, StorageError> {
    let schema = crate::database::quoted_schema(&write.spill.database.schema);
    let sql = format!(
        r#"SELECT row_count, logical_bytes, batch_digest
            FROM {schema}."native_generation_spill_batches"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND relation = $3
              AND batch_sequence = $4"#
    );
    let Some(row) = audited_query(sql)
        .bind(write.spill.project_id.as_str())
        .bind(write.spill.generation_id.as_str())
        .bind(identity.relation.as_str())
        .bind(sequence)
        .fetch_optional(&mut **write.transaction)
        .await
        .map_err(|_| database_error("spill-read-batch"))?
    else {
        return Ok(false);
    };
    let row_count = row
        .try_get::<i32, _>(0)
        .map_err(|_| corrupt("spill_batch_row_count"))?;
    let logical_bytes = row
        .try_get::<i64, _>(1)
        .map_err(|_| corrupt("spill_batch_logical_bytes"))?;
    let digest = row
        .try_get::<String, _>(2)
        .map_err(|_| corrupt("spill_batch_digest"))?;
    if usize::try_from(row_count).ok() != Some(identity.rows)
        || u64::try_from(logical_bytes).ok() != Some(identity.logical_bytes)
        || digest != identity.digest.as_str()
    {
        return Err(StorageError::GenerationSpillConflict);
    }
    Ok(true)
}

async fn existing_fact_batches(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    sequences: &[i64],
) -> Result<StoredFactBatches, StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let sql = format!(
        r#"SELECT batch_sequence, relation, row_count, logical_bytes, batch_digest
            FROM {schema}."native_generation_spill_batches"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND batch_sequence = ANY(CAST($3 AS bigint[]))
              AND relation <> 'extracted_files'
            ORDER BY batch_sequence, relation"#
    );
    let rows = audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(sequences)
        .fetch_all(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-read-fact-batches"))?;
    let mut existing = StoredFactBatches::new();
    for row in rows {
        let sequence = row
            .try_get::<i64, _>(0)
            .map_err(|_| corrupt("spill_batch_sequence"))?;
        let relation = row
            .try_get::<String, _>(1)
            .map_err(|_| corrupt("spill_batch_relation"))?;
        let row_count = row
            .try_get::<i32, _>(2)
            .map_err(|_| corrupt("spill_batch_row_count"))?;
        let logical_bytes = row
            .try_get::<i64, _>(3)
            .map_err(|_| corrupt("spill_batch_logical_bytes"))?;
        let digest = row
            .try_get::<String, _>(4)
            .map_err(|_| corrupt("spill_batch_digest"))?;
        existing.entry(sequence).or_default().insert(
            relation,
            StoredFactPayload {
                row_count,
                logical_bytes,
                digest,
            },
        );
    }
    Ok(existing)
}

async fn insert_batch_ledger(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    input: SpillBatchLedgerInput<'_>,
) -> Result<(), StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let sql = format!(
        r#"INSERT INTO {schema}."native_generation_spill_batches" (
                project_id, generation_id, relation, batch_sequence,
                row_count, logical_bytes, batch_digest
            ) VALUES (CAST($1 AS uuid), CAST($2 AS uuid), $3, $4, $5, $6, $7)"#
    );
    audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(input.relation.as_str())
        .bind(input.sequence)
        .bind(
            i32::try_from(input.rows).map_err(|_| StorageError::InvalidInput {
                field: "spill_batch_rows",
            })?,
        )
        .bind(as_i64(input.logical_bytes, "spill_batch_bytes")?)
        .bind(input.digest.as_str())
        .execute(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-insert-batch"))?;
    Ok(())
}

async fn insert_fact_batch_ledgers(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    batches: &[(i64, &NativeGenerationSpillFactBatch)],
) -> Result<(), StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let payload_count = batches
        .iter()
        .try_fold(0_usize, |count, (_, batch)| {
            count.checked_add(batch.payloads.len())
        })
        .ok_or(StorageError::InvalidInput {
            field: "spill_fact_batches",
        })?;
    let mut relations = Vec::new();
    let mut sequences = Vec::new();
    let mut row_counts = Vec::new();
    let mut logical_bytes = Vec::new();
    let mut digests = Vec::new();
    relations
        .try_reserve_exact(payload_count)
        .map_err(|_| database_error("spill-fact-ledger-reserve"))?;
    sequences
        .try_reserve_exact(payload_count)
        .map_err(|_| database_error("spill-fact-ledger-reserve"))?;
    row_counts
        .try_reserve_exact(payload_count)
        .map_err(|_| database_error("spill-fact-ledger-reserve"))?;
    logical_bytes
        .try_reserve_exact(payload_count)
        .map_err(|_| database_error("spill-fact-ledger-reserve"))?;
    digests
        .try_reserve_exact(payload_count)
        .map_err(|_| database_error("spill-fact-ledger-reserve"))?;
    for (sequence, batch) in batches {
        for payload in &batch.payloads {
            relations.push(payload.relation.as_str().to_owned());
            sequences.push(*sequence);
            row_counts.push(i32::try_from(payload.row_count).map_err(|_| {
                StorageError::InvalidInput {
                    field: "spill_batch_rows",
                }
            })?);
            logical_bytes.push(as_i64(payload.logical_bytes, "spill_batch_bytes")?);
            digests.push(payload.digest.as_str().to_owned());
        }
    }
    let sql = format!(
        r#"INSERT INTO {schema}."native_generation_spill_batches" (
                project_id, generation_id, relation, batch_sequence,
                row_count, logical_bytes, batch_digest
            )
            SELECT CAST($1 AS uuid), CAST($2 AS uuid), input.relation,
                   input.batch_sequence, input.row_count, input.logical_bytes,
                   input.batch_digest
            FROM unnest(
                CAST($3 AS text[]), CAST($4 AS bigint[]), CAST($5 AS integer[]),
                CAST($6 AS bigint[]), CAST($7 AS text[])
            ) AS input(
                relation, batch_sequence, row_count, logical_bytes, batch_digest
            )"#
    );
    let inserted = audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(relations)
        .bind(sequences)
        .bind(row_counts)
        .bind(logical_bytes)
        .bind(digests)
        .execute(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-insert-fact-batches"))?;
    if inserted.rows_affected() != usize_to_u64(payload_count) {
        return Err(database_error("spill-fact-ledger-count-mismatch"));
    }
    Ok(())
}

async fn ensure_opaque_batch_range_available(
    write: &mut SpillWrite<'_, '_>,
    sequence: i64,
    rows: usize,
) -> Result<(), StorageError> {
    let schema = crate::database::quoted_schema(&write.spill.database.schema);
    let sql = format!(
        r#"SELECT EXISTS (
                SELECT 1
                FROM {schema}."native_generation_spill_batches"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND relation = 'extracted_files'
                  AND int8range(batch_sequence, batch_sequence + row_count, '[)')
                      && int8range($3, $3 + $4, '[)')
            )"#
    );
    let overlap = audited_query(sql)
        .bind(write.spill.project_id.as_str())
        .bind(write.spill.generation_id.as_str())
        .bind(sequence)
        .bind(i64::try_from(rows).map_err(|_| StorageError::InvalidInput {
            field: "spill_batch_rows",
        })?)
        .fetch_one(&mut **write.transaction)
        .await
        .map_err(|_| database_error("spill-read-batch-range"))?
        .try_get::<bool, _>(0)
        .map_err(|_| corrupt("spill_batch_range"))?;
    if overlap {
        Err(StorageError::GenerationSpillConflict)
    } else {
        Ok(())
    }
}

async fn insert_extracted_rows(
    write: &mut SpillWrite<'_, '_>,
    sequence: i64,
    batch: &NativeGenerationSpillExtractedBatch,
) -> Result<(), StorageError> {
    let arrays = extracted_row_arrays(batch)?;
    let inline = insert_inline_extracted_rows(write, sequence, arrays.inline).await?;
    let cached = insert_cached_extracted_rows(write, sequence, arrays.cached).await?;
    if inline.saturating_add(cached) != usize_to_u64(batch.rows.len()) {
        return Err(StorageError::GenerationSpillConflict);
    }
    Ok(())
}

async fn insert_inline_extracted_rows(
    write: &mut SpillWrite<'_, '_>,
    sequence: i64,
    arrays: InlineExtractedArrays,
) -> Result<u64, StorageError> {
    let schema = crate::database::quoted_schema(&write.spill.database.schema);
    if arrays.ordinals.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        r#"INSERT INTO {schema}."native_generation_spill_rows" (
                project_id, generation_id, relation, batch_sequence,
                row_ordinal, sort_key, payload, payload_digest
            )
            SELECT CAST($1 AS uuid), CAST($2 AS uuid), 'extracted_files', $3,
                   rows.row_ordinal, rows.sort_key, rows.payload, rows.payload_digest
            FROM unnest(
                CAST($4 AS integer[]), CAST($5 AS bytea[]),
                CAST($6 AS bytea[]), CAST($7 AS text[])
            ) AS rows(row_ordinal, sort_key, payload, payload_digest)"#
    );
    let inserted = audited_query(sql)
        .bind(write.spill.project_id.as_str())
        .bind(write.spill.generation_id.as_str())
        .bind(sequence)
        .bind(arrays.ordinals)
        .bind(arrays.sort_keys)
        .bind(arrays.payloads)
        .bind(arrays.payload_digests)
        .execute(&mut **write.transaction)
        .await
        .map_err(|_| database_error("spill-insert-rows"))?;
    Ok(inserted.rows_affected())
}

async fn insert_cached_extracted_rows(
    write: &mut SpillWrite<'_, '_>,
    sequence: i64,
    arrays: CachedExtractedArrays,
) -> Result<u64, StorageError> {
    let schema = crate::database::quoted_schema(&write.spill.database.schema);
    if arrays.ordinals.is_empty() {
        return Ok(0);
    }
    let sql = format!(
        r#"INSERT INTO {schema}."native_generation_spill_rows" (
                project_id, generation_id, relation, batch_sequence,
                row_ordinal, sort_key, payload, payload_digest,
                cache_extractor_contract_digest, cache_path_digest,
                cache_language, cache_content_hash, cache_payload_bytes
            )
            SELECT CAST($1 AS uuid), CAST($2 AS uuid), 'extracted_files', $3,
                   input.row_ordinal, input.sort_key, NULL, input.payload_digest,
                   input.contract, input.path_digest, input.language,
                   input.content_hash, cache.payload_bytes
            FROM unnest(
                CAST($4 AS integer[]), CAST($5 AS bytea[]), CAST($6 AS text[]),
                CAST($7 AS text[]), CAST($8 AS text[]), CAST($9 AS text[]),
                CAST($10 AS text[]), CAST($11 AS bigint[]), CAST($12 AS text[]),
                CAST($13 AS bigint[])
            ) AS input(
                row_ordinal, sort_key, contract, path_digest, normalized_path,
                language, content_hash, source_bytes, payload_digest, payload_bytes
            )
            INNER JOIN {schema}."native_parse_cache" AS cache
              ON cache.project_id = CAST($1 AS uuid)
             AND cache.extractor_contract_digest = input.contract
             AND cache.path_digest = input.path_digest
             AND cache.normalized_path = input.normalized_path
             AND cache.language = input.language
             AND cache.content_hash = input.content_hash
             AND cache.source_bytes = input.source_bytes
             AND cache.payload_digest = input.payload_digest
             AND cache.payload_bytes = input.payload_bytes"#
    );
    let inserted = audited_query(sql)
        .bind(write.spill.project_id.as_str())
        .bind(write.spill.generation_id.as_str())
        .bind(sequence)
        .bind(arrays.ordinals)
        .bind(arrays.sort_keys)
        .bind(arrays.contracts)
        .bind(arrays.path_digests)
        .bind(arrays.paths)
        .bind(arrays.languages)
        .bind(arrays.content_hashes)
        .bind(arrays.source_bytes)
        .bind(arrays.payload_digests)
        .bind(arrays.payload_bytes)
        .execute(&mut **write.transaction)
        .await
        .map_err(|_| database_error("spill-insert-cached-rows"))?;
    Ok(inserted.rows_affected())
}

async fn increment_spill_totals(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    totals: SpillTotalsDelta,
) -> Result<(), StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let SpillTotalsDelta {
        logical_bytes,
        rows,
        extracted_files,
    } = totals;
    let sql = format!(
        r#"UPDATE {schema}."native_generation_spills"
            SET logical_bytes = logical_bytes + $3,
                raw_rows = raw_rows + $4,
                extracted_files = extracted_files + $5,
                updated_at = clock_timestamp()
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#
    );
    let updated = audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(as_i64(logical_bytes, "spill_batch_bytes")?)
        .bind(as_i64(rows, "spill_batch_rows")?)
        .bind(as_i64(extracted_files, "spill_extracted_files")?)
        .execute(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-update-run"))?;
    if updated.rows_affected() != 1 {
        return Err(database_error("spill-update-run-count"));
    }
    Ok(())
}

async fn select_extracted_page_window(
    write: &mut SpillWrite<'_, '_>,
    cursor: ExtractedPageCursor,
    maximum_bytes: u64,
) -> Result<Option<ExtractedPageWindow>, StorageError> {
    let schema = crate::database::quoted_schema(&write.spill.database.schema);
    let ExtractedPageCursor {
        after,
        minimum_batch_sequence,
    } = cursor;
    // Select only inline row metadata first. Keeping the TOAST-able payload out
    // avoids carrying every remaining file through a cumulative page window.
    let sql = format!(
        r#"SELECT batch_sequence + row_ordinal::bigint AS row_sequence,
                  logical_bytes
           FROM {schema}."native_generation_spill_rows"
           WHERE project_id = CAST($1 AS uuid)
             AND generation_id = CAST($2 AS uuid)
             AND relation = 'extracted_files'
             AND batch_sequence + row_ordinal::bigint > $3
             AND batch_sequence >= $4
           ORDER BY batch_sequence, row_ordinal
           LIMIT $5"#
    );
    let metadata = audited_query(sql)
        .bind(write.spill.project_id.as_str())
        .bind(write.spill.generation_id.as_str())
        .bind(after)
        .bind(minimum_batch_sequence)
        .bind(MAXIMUM_PAGE_ROWS)
        .fetch_all(&mut **write.transaction)
        .await
        .map_err(|_| database_error("spill-read-page"))?;
    let mut selected_bytes = 0_u64;
    let mut row_count = 0_usize;
    let mut end_sequence = None;
    for row in metadata {
        let sequence = row
            .try_get::<i64, _>(0)
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| corrupt("spill_batch_sequence"))?;
        let logical_bytes = row
            .try_get::<i64, _>(1)
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| corrupt("spill_logical_bytes"))?;
        let next_bytes = selected_bytes
            .checked_add(logical_bytes)
            .ok_or_else(|| corrupt("spill_logical_bytes"))?;
        if end_sequence.is_some() && next_bytes > maximum_bytes {
            break;
        }
        selected_bytes = next_bytes;
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| corrupt("spill_page_rows"))?;
        end_sequence = Some(sequence);
    }
    Ok(end_sequence.map(|end_sequence| ExtractedPageWindow {
        end_sequence,
        row_count,
    }))
}

async fn load_extracted_page_payloads(
    write: &mut SpillWrite<'_, '_>,
    cursor: ExtractedPageCursor,
    window: &ExtractedPageWindow,
) -> Result<Vec<(u64, NativeGenerationSpillRow)>, StorageError> {
    let schema = crate::database::quoted_schema(&write.spill.database.schema);
    let ExtractedPageCursor {
        after,
        minimum_batch_sequence,
    } = cursor;
    let sql = format!(
        r#"SELECT batch_sequence + row_ordinal::bigint AS row_sequence,
                  spill.sort_key, COALESCE(spill.payload, cache.payload),
                  spill.payload_digest
           FROM {schema}."native_generation_spill_rows" AS spill
           LEFT JOIN {schema}."native_parse_cache" AS cache
             ON spill.payload IS NULL
            AND cache.project_id = spill.project_id
            AND cache.extractor_contract_digest = spill.cache_extractor_contract_digest
            AND cache.path_digest = spill.cache_path_digest
            AND cache.language = spill.cache_language
            AND cache.content_hash = spill.cache_content_hash
            AND cache.payload_digest = spill.payload_digest
            AND cache.payload_bytes = spill.cache_payload_bytes
           WHERE spill.project_id = CAST($1 AS uuid)
             AND spill.generation_id = CAST($2 AS uuid)
             AND spill.relation = 'extracted_files'
             AND spill.batch_sequence + spill.row_ordinal::bigint > $3
             AND spill.batch_sequence >= $4
             AND spill.batch_sequence <= $5
             AND spill.batch_sequence + spill.row_ordinal::bigint <= $5
           ORDER BY spill.batch_sequence, spill.row_ordinal"#
    );
    let rows = audited_query(sql)
        .bind(write.spill.project_id.as_str())
        .bind(write.spill.generation_id.as_str())
        .bind(after)
        .bind(minimum_batch_sequence)
        .bind(as_i64(window.end_sequence, "spill_page_cursor")?)
        .fetch_all(&mut **write.transaction)
        .await
        .map_err(|_| database_error("spill-read-page"))?;
    if rows.len() != window.row_count {
        return Err(corrupt("spill_page_rows"));
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(rows.len())
        .map_err(|_| database_error("spill-read-page-reserve"))?;
    for row in rows {
        let sequence = row
            .try_get::<i64, _>(0)
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| corrupt("spill_batch_sequence"))?;
        let sort_key = row
            .try_get::<Vec<u8>, _>(1)
            .map_err(|_| corrupt("spill_sort_key"))?;
        let payload = row
            .try_get::<Vec<u8>, _>(2)
            .map_err(|_| corrupt("spill_payload"))?;
        let raw_digest = row
            .try_get::<String, _>(3)
            .map_err(|_| corrupt("spill_payload_digest"))?;
        let spill_row = NativeGenerationSpillRow::new(sort_key, payload)?;
        if spill_row.payload_digest.as_str() != raw_digest {
            return Err(corrupt("spill_payload_digest"));
        }
        decoded.push((sequence, spill_row));
    }
    Ok(decoded)
}

/// One new extracted-file batch and the accounting that must admit it.
struct NewExtractedBatch<'batch> {
    report: NativeGenerationSpillReport,
    sequence: i64,
    batch: &'batch NativeGenerationSpillExtractedBatch,
}

/// Store one extracted-file batch that has no exact retry ledger yet: prove its
/// sequence window is free and its quota fits, then write the ledger, its rows
/// and the running totals inside the caller's fenced transaction.
async fn write_new_extracted_batch(
    write: &mut SpillWrite<'_, '_>,
    input: NewExtractedBatch<'_>,
) -> Result<(), StorageError> {
    let NewExtractedBatch {
        report,
        sequence,
        batch,
    } = input;
    ensure_opaque_batch_range_available(write, sequence, batch.rows.len()).await?;
    let batch_rows = usize_to_u64(batch.rows.len());
    admit_spill_quota(report, batch.logical_bytes, batch_rows)?;
    insert_batch_ledger(
        write.spill,
        write.transaction,
        SpillBatchLedgerInput {
            relation: NativeGenerationSpillRelation::ExtractedFiles,
            sequence,
            rows: batch.rows.len(),
            logical_bytes: batch.logical_bytes,
            digest: &batch.digest,
        },
    )
    .await?;
    insert_extracted_rows(write, sequence, batch).await?;
    increment_spill_totals(
        write.spill,
        write.transaction,
        SpillTotalsDelta {
            logical_bytes: batch.logical_bytes,
            rows: batch_rows,
            extracted_files: batch_rows,
        },
    )
    .await
}

/// Run phase, quota report and stored retry ledgers that decide which fact
/// batches of one group are exact no-ops and which must be inserted.
#[derive(Clone, Copy)]
struct FactAdmissionInput<'input> {
    phase: NativeGenerationSpillPhase,
    report: NativeGenerationSpillReport,
    existing: &'input StoredFactBatches,
}

/// Per-batch write outcomes plus the new batches and the totals they add.
struct FactAdmission<'batches> {
    writes: Vec<NativeGenerationSpillWrite>,
    admitted: Vec<(i64, &'batches NativeGenerationSpillFactBatch)>,
    logical_bytes: u64,
    rows: u64,
}

/// Classify every batch of one group in order: exact retries stay no-ops, while
/// new batches require the resolving phase and must fit the cumulative quota.
fn admit_fact_batches<'batches>(
    input: FactAdmissionInput<'_>,
    sequences: Vec<i64>,
    batches: &'batches [NativeGenerationSpillFactBatch],
) -> Result<FactAdmission<'batches>, StorageError> {
    let mut admission = FactAdmission {
        writes: Vec::new(),
        admitted: Vec::new(),
        logical_bytes: 0,
        rows: 0,
    };
    admission
        .writes
        .try_reserve_exact(batches.len())
        .map_err(|_| database_error("spill-fact-write-reserve"))?;
    admission
        .admitted
        .try_reserve_exact(batches.len())
        .map_err(|_| database_error("spill-fact-write-reserve"))?;
    for (sequence, batch) in sequences.into_iter().zip(batches) {
        if stored_fact_batch_is_exact(input.existing.get(&sequence), batch)? {
            admission
                .writes
                .push(NativeGenerationSpillWrite::AlreadyPresent);
            continue;
        }
        if input.phase != NativeGenerationSpillPhase::Resolving {
            return Err(StorageError::InvalidGenerationTransition {
                actual: input.phase.as_str().to_owned(),
                requested: NativeGenerationSpillPhase::Resolving.as_str(),
            });
        }
        admission.logical_bytes = admission
            .logical_bytes
            .checked_add(batch.logical_bytes)
            .ok_or(StorageError::InvalidInput {
                field: "spill_fact_batch_bytes",
            })?;
        admission.rows =
            admission
                .rows
                .checked_add(batch.row_count)
                .ok_or(StorageError::InvalidInput {
                    field: "spill_fact_batch_rows",
                })?;
        admit_spill_quota(input.report, admission.logical_bytes, admission.rows)?;
        admission.admitted.push((sequence, batch));
        admission.writes.push(NativeGenerationSpillWrite::Inserted);
    }
    Ok(admission)
}

/// Write the retry ledgers, every typed fact relation and the running totals
/// for the admitted batches of one group; a group with nothing new writes nothing.
async fn insert_admitted_fact_batches(
    write: &mut SpillWrite<'_, '_>,
    admission: &FactAdmission<'_>,
) -> Result<(), StorageError> {
    if admission.admitted.is_empty() {
        return Ok(());
    }
    let schema = crate::database::quoted_schema(&write.spill.database.schema);
    insert_fact_batch_ledgers(write.spill, write.transaction, &admission.admitted).await?;
    for relation in NativeGenerationSpillRelation::FACTS {
        insert_typed_fact_payloads(
            write.transaction,
            SpillScope {
                schema: &schema,
                project_id: &write.spill.project_id,
                generation_id: &write.spill.generation_id,
            },
            TypedFactPayloads {
                relation,
                batches: &admission.admitted,
            },
        )
        .await?;
    }
    increment_spill_totals(
        write.spill,
        write.transaction,
        SpillTotalsDelta {
            logical_bytes: admission.logical_bytes,
            rows: admission.rows,
            extracted_files: 0,
        },
    )
    .await
}

/// Open one pooled transaction already bounded by the spill deadline and
/// locked to its exact staging-generation fence.
async fn begin_spill_transaction(
    spill: &NativeGenerationSpill,
    operation: &'static str,
) -> Result<sqlx_postgres::PgTransaction<'static>, StorageError> {
    let mut transaction = spill
        .database
        .pool
        .begin()
        .await
        .map_err(|_| database_error(operation))?;
    prepare_transaction(spill, &mut transaction).await?;
    Ok(transaction)
}

async fn prepare_transaction(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
) -> Result<(), StorageError> {
    set_local_statement_timeout(transaction, spill.statement_timeout)
        .await
        .map_err(|()| database_error("spill-statement-timeout"))?;
    lock_staging_generation_fence(
        transaction,
        &spill.database.schema,
        StagingFenceTarget {
            fence: &spill.fence,
            project_id: &spill.project_id,
            generation_id: &spill.generation_id,
            sequence: spill.generation_sequence,
        },
    )
    .await
}

async fn commit_transaction(
    spill: &NativeGenerationSpill,
    mut transaction: sqlx_postgres::PgTransaction<'_>,
    operation: &'static str,
) -> Result<(), StorageError> {
    check_staging_generation_fence(
        &mut transaction,
        &spill.database.schema,
        StagingFenceTarget {
            fence: &spill.fence,
            project_id: &spill.project_id,
            generation_id: &spill.generation_id,
            sequence: spill.generation_sequence,
        },
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| database_error(operation))
}

async fn load_run_for_update(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
) -> Result<(NativeGenerationSpillPhase, NativeGenerationSpillReport), StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let sql = format!(
        r#"SELECT phase, logical_bytes, raw_rows, extracted_files,
                   maximum_bytes, maximum_rows
            FROM {schema}."native_generation_spills"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND generation_sequence = $3
            FOR UPDATE"#
    );
    let row = audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(spill.generation_sequence)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-read-run"))?
        .ok_or(StorageError::GenerationNotFound)?;
    let phase = row
        .try_get::<String, _>(0)
        .map_err(|_| corrupt("native_generation_spill_phase"))
        .and_then(|raw| NativeGenerationSpillPhase::parse(&raw))?;
    let logical_bytes = read_u64(&row, "logical_bytes", "spill_logical_bytes")?;
    let raw_rows = read_u64(&row, "raw_rows", "spill_raw_rows")?;
    let extracted_files = read_u64(&row, "extracted_files", "spill_extracted_files")?;
    let maximum_bytes = read_u64(&row, "maximum_bytes", "maximum_spill_bytes")?;
    let maximum_rows = read_u64(&row, "maximum_rows", "maximum_spill_rows")?;
    Ok((
        phase,
        NativeGenerationSpillReport {
            state: phase.into(),
            logical_bytes,
            raw_rows,
            extracted_files,
            maximum_bytes,
            maximum_rows,
        },
    ))
}

/// One spill with the open transaction its storage step runs inside.
struct SpillWrite<'write, 'transaction> {
    spill: &'write NativeGenerationSpill,
    transaction: &'write mut sqlx_postgres::PgTransaction<'transaction>,
}

/// One reduced canonical partition and the rows it inserted.
#[derive(Clone, Copy)]
struct ReducedPartition<'partition> {
    cursor: &'partition NativeGenerationCanonicalCursor,
    inserted: u64,
}

/// Deterministic PostgreSQL-side canonical reduction for one spilled generation.
///
/// Canonicalisation walks a cursor over relations and partitions, which is a
/// separate responsibility from admitting spill rows and reading them back.
async fn begin_canonicalization(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    phase: NativeGenerationSpillPhase,
) -> Result<(), StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    if phase == NativeGenerationSpillPhase::Canonicalizing {
        return Ok(());
    }
    if phase != NativeGenerationSpillPhase::Sealed {
        return Err(StorageError::InvalidGenerationTransition {
            actual: phase.as_str().to_owned(),
            requested: NativeGenerationSpillPhase::Canonicalizing.as_str(),
        });
    }
    let sql = format!(
        r#"UPDATE {schema}."native_generation_spills"
            SET phase = 'canonicalizing', updated_at = clock_timestamp()
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND phase = 'sealed'"#
    );
    let updated = audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .execute(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-begin-canonicalization"))?;
    if updated.rows_affected() != 1 {
        return Err(StorageError::GenerationSpillConflict);
    }
    Ok(())
}

async fn load_canonical_cursor(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
) -> Result<NativeGenerationCanonicalCursor, StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let sql = format!(
        r#"SELECT canonical_relation, canonical_partition
            FROM {schema}."native_generation_spills"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)"#
    );
    let row = audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-read-canonical-cursor"))?;
    let relation = row
        .try_get::<String, _>(0)
        .map_err(|_| corrupt("spill_canonical_relation"))
        .and_then(|raw| parse_fact_relation(&raw))?;
    let partition = row
        .try_get::<i32, _>(1)
        .ok()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value < CANONICAL_PARTITIONS)
        .ok_or_else(|| corrupt("spill_canonical_partition"))?;
    let lower_bucket = i32::from(partition) * i32::from(BUCKETS_PER_CANONICAL_PARTITION);
    let partition_count =
        CANONICAL_PARTITIONS_PER_TRANSACTION.min(CANONICAL_PARTITIONS.saturating_sub(partition));
    Ok(NativeGenerationCanonicalCursor {
        relation,
        partition,
        lower_bucket,
        upper_bucket: lower_bucket
            + i32::from(partition_count) * i32::from(BUCKETS_PER_CANONICAL_PARTITION)
            - 1,
    })
}

async fn reduce_canonical_partition(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    cursor: &NativeGenerationCanonicalCursor,
) -> Result<u64, StorageError> {
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let statements = canonical_partition_sql(&schema, cursor.relation);
    if audited_query(statements.conflict)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(cursor.lower_bucket)
        .bind(cursor.upper_bucket)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|error| classify_statement_error(&error, "spill-detect-canonical-conflict"))?
        .is_some()
    {
        return Err(StorageError::GenerationSpillConflict);
    }
    let inserted = audited_query(statements.insert)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(cursor.lower_bucket)
        .bind(cursor.upper_bucket)
        .execute(&mut **transaction)
        .await
        .map_err(|error| {
            // Any other rejection is a canonical constraint violation by the
            // spilled facts themselves.
            match classify_statement_error(&error, "spill-insert-canonical-partition") {
                timeout @ StorageError::StatementTimeout { .. } => timeout,
                _ => StorageError::GenerationSpillConflict,
            }
        })?
        .rows_affected();
    let delete_sql = format!(
        r#"DELETE FROM {schema}."{table}"
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND bucket BETWEEN $3 AND $4"#,
        table = statements.raw_table,
    );
    audited_query(delete_sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(cursor.lower_bucket)
        .bind(cursor.upper_bucket)
        .execute(&mut **transaction)
        .await
        .map_err(|error| classify_statement_error(&error, "spill-delete-canonicalized-raw"))?;
    Ok(inserted)
}

/// Refresh planner statistics for one table this generation just filled.
///
/// The partition queries are only bounded while PostgreSQL plans them from
/// statistics that include this generation. A never-analyzed table, or a
/// sample of only earlier or failed generations, makes the planner estimate
/// this generation's `(project_id, generation_id)` prefix at about one row, so
/// an index matching only that prefix looks as cheap as the exact key. Each
/// probe of such an index then walks the whole generation: a full rebuild of a
/// few hundred thousand symbols validated a single edge or document partition
/// for longer than the spill statement timeout. Autovacuum may sample these
/// tables at any point, so the reduce does not rely on it. The sample counts
/// this transaction's own rows, so the statistics describe the whole table.
async fn analyze_planner_statistics(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    target: Option<(&'static str, &'static str)>,
) -> Result<(), StorageError> {
    let Some((table, columns)) = target else {
        return Ok(());
    };
    let schema = crate::database::quoted_schema(&spill.database.schema);
    audited_query(format!(r#"ANALYZE {schema}."{table}" ({columns})"#))
        .execute(&mut **transaction)
        .await
        .map_err(|error| classify_statement_error(&error, "spill-analyze-planner-statistics"))?;
    Ok(())
}

async fn advance_canonical_cursor(
    spill: &NativeGenerationSpill,
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    reduced: ReducedPartition<'_>,
) -> Result<bool, StorageError> {
    let ReducedPartition { cursor, inserted } = reduced;
    let schema = crate::database::quoted_schema(&spill.database.schema);
    let next_partition = cursor
        .partition
        .saturating_add(CANONICAL_PARTITIONS_PER_TRANSACTION)
        .min(CANONICAL_PARTITIONS);
    let next_relation = next_fact_relation(cursor.relation);
    let complete = next_partition == CANONICAL_PARTITIONS && next_relation.is_none();
    let (stored_relation, stored_partition, next_phase) = if next_partition < CANONICAL_PARTITIONS {
        (Some(cursor.relation), next_partition, "canonicalizing")
    } else if let Some(next_relation) = next_relation {
        (Some(next_relation), 0, "canonicalizing")
    } else {
        (None, CANONICAL_PARTITIONS, "canonicalized")
    };
    let sql = format!(
        r#"UPDATE {schema}."native_generation_spills"
            SET phase = $3, canonical_relation = $4, canonical_partition = $5,
                canonical_rows = canonical_rows + $6,
                updated_at = clock_timestamp()
            WHERE project_id = CAST($1 AS uuid)
              AND generation_id = CAST($2 AS uuid)
              AND phase = 'canonicalizing'"#
    );
    let updated = audited_query(sql)
        .bind(spill.project_id.as_str())
        .bind(spill.generation_id.as_str())
        .bind(next_phase)
        .bind(stored_relation.map(NativeGenerationSpillRelation::as_str))
        .bind(i32::from(stored_partition))
        .bind(as_i64(inserted, "spill_canonical_rows")?)
        .execute(&mut **transaction)
        .await
        .map_err(|_| database_error("spill-advance-canonicalization"))?;
    if updated.rows_affected() != 1 {
        return Err(StorageError::GenerationSpillConflict);
    }
    Ok(complete)
}

/// One canonical relation's contribution to a spilled batch digest.
///
/// The begin/push order is part of the generation digest, so each relation owns
/// its own sequence rather than having it copied per relation.
trait SpillDigestRow {
    const RELATION: NativeGenerationSpillRelation;

    fn begin(digest: &mut LogicalDigestBuilder, rows: u64);
    fn push(&self, digest: &mut LogicalDigestBuilder);
}

impl SpillDigestRow for FileInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Files;

    fn begin(digest: &mut LogicalDigestBuilder, rows: u64) {
        digest.begin_files(rows);
    }

    fn push(&self, digest: &mut LogicalDigestBuilder) {
        digest.push_file(self);
    }
}

impl SpillDigestRow for SymbolInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Symbols;

    fn begin(digest: &mut LogicalDigestBuilder, rows: u64) {
        digest.begin_symbols(rows);
    }

    fn push(&self, digest: &mut LogicalDigestBuilder) {
        digest.push_symbol(self);
        // Centrality scores are optional, so presence is encoded explicitly
        // rather than being omitted from the digest stream.
        for score in [self.betweenness_ppb, self.pagerank_ppb] {
            if let Some(score) = score {
                let mut encoded = [0_u8; 5];
                encoded[0] = 1;
                encoded[1..].copy_from_slice(&score.to_be_bytes());
                digest.push_encoded_row(&encoded);
            } else {
                digest.push_encoded_row(&[0]);
            }
        }
    }
}

impl SpillDigestRow for EdgeInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Edges;

    fn begin(digest: &mut LogicalDigestBuilder, rows: u64) {
        digest.begin_edges(rows);
    }

    fn push(&self, digest: &mut LogicalDigestBuilder) {
        digest.push_edge(self);
    }
}

impl SpillDigestRow for ReferenceInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::References;

    fn begin(digest: &mut LogicalDigestBuilder, rows: u64) {
        digest.begin_references(rows);
    }

    fn push(&self, digest: &mut LogicalDigestBuilder) {
        digest.push_reference(self);
    }
}

impl SpillDigestRow for NumericalSiteInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::NumericalSites;

    fn begin(digest: &mut LogicalDigestBuilder, rows: u64) {
        digest.begin_numerical_sites(rows);
    }

    fn push(&self, digest: &mut LogicalDigestBuilder) {
        digest.push_numerical_site(self);
    }
}

impl SpillDigestRow for CanonicalSearchDocument {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Documents;

    fn begin(digest: &mut LogicalDigestBuilder, rows: u64) {
        digest.begin_documents(rows);
    }

    fn push(&self, digest: &mut LogicalDigestBuilder) {
        digest.push_document(self);
    }
}

fn encode_relation_payload<T: SpillDigestRow>(
    rows: &[T],
) -> Result<Option<NativeGenerationSpillRelationPayload>, GenerationValidationError> {
    let mut digest = LogicalDigestBuilder::new(GenerationDigestVersion::CURRENT);
    T::begin(&mut digest, usize_to_u64(rows.len()));
    for row in rows {
        row.push(&mut digest);
    }
    finish_relation_payload(T::RELATION, rows.len(), digest)
}

fn finish_relation_payload(
    relation: NativeGenerationSpillRelation,
    row_count: usize,
    digest: LogicalDigestBuilder,
) -> Result<Option<NativeGenerationSpillRelationPayload>, GenerationValidationError> {
    if row_count == 0 {
        return Ok(None);
    }
    if row_count > MAXIMUM_BATCH_ROWS {
        return Err(StorageError::InvalidInput {
            field: "spill_fact_batch_rows",
        }
        .into());
    }
    let (digest, logical_bytes) = digest.finish_with_encoded_bytes();
    if logical_bytes == 0 || logical_bytes > MAXIMUM_BATCH_BYTES {
        return Err(StorageError::InvalidInput {
            field: "spill_fact_batch_bytes",
        }
        .into());
    }
    Ok(Some(NativeGenerationSpillRelationPayload {
        relation,
        row_count,
        logical_bytes,
        digest,
    }))
}

fn typed_fact_insert_sql(
    schema: &str,
    relation: NativeGenerationSpillRelation,
) -> Result<String, StorageError> {
    match relation {
        NativeGenerationSpillRelation::ExtractedFiles => Err(StorageError::InvalidInput {
            field: "spill_fact_relation",
        }),
        NativeGenerationSpillRelation::Files => Ok(typed_file_insert_sql(schema)),
        NativeGenerationSpillRelation::Symbols => Ok(typed_symbol_insert_sql(schema)),
        NativeGenerationSpillRelation::Edges => Ok(typed_edge_insert_sql(schema)),
        NativeGenerationSpillRelation::References => Ok(typed_reference_insert_sql(schema)),
        NativeGenerationSpillRelation::NumericalSites => Ok(typed_numerical_insert_sql(schema)),
        NativeGenerationSpillRelation::Documents => Ok(typed_document_insert_sql(schema)),
    }
}

fn typed_file_insert_sql(schema: &str) -> String {
    spill_typed_copy_sql(
        schema,
        "native_generation_spill_files",
        "file_id, normalized_path, language, content_hash, byte_size, parse_status",
    )
}

fn typed_symbol_insert_sql(schema: &str) -> String {
    spill_typed_copy_sql(
        schema,
        "native_generation_spill_symbols",
        "symbol_id, file_id, symbol_kind, qualified_name, signature, start_byte, end_byte, start_line, end_line, structural_digest, visibility, exported, default_export, async_symbol, static_member, declaration_only, betweenness, pagerank",
    )
}

fn typed_edge_insert_sql(schema: &str) -> String {
    spill_typed_copy_sql(
        schema,
        "native_generation_spill_edges",
        "source_symbol_id, target_symbol_id, edge_kind, confidence, provenance, site_count",
    )
}

fn typed_reference_insert_sql(schema: &str) -> String {
    spill_typed_copy_sql(
        schema,
        "native_generation_spill_references",
        "file_id, owner_symbol_id, target_symbol_id, reference_name, reference_kind, start_byte, end_byte, confidence, resolution_provenance, site_count, span_precision",
    )
}

fn typed_numerical_insert_sql(schema: &str) -> String {
    spill_typed_copy_sql(
        schema,
        "native_generation_spill_numerical_sites",
        "numerical_site_id, file_id, owner_symbol_id, start_byte, end_byte, start_line, end_line, operation, hazard, precision, expression_digest, confidence_ppm, provenance, evidence_level, unknowns",
    )
}

fn typed_document_insert_sql(schema: &str) -> String {
    spill_typed_copy_sql(
        schema,
        "native_generation_spill_documents",
        "document_id, file_id, symbol_id, path, language, document_kind, qualified_name, code, natural_text, metadata, metadata_json",
    )
}

fn spill_typed_copy_sql(schema: &str, table: &str, columns: &str) -> String {
    format!(
        r#"COPY {schema}."{table}" (
                project_id, generation_id, batch_sequence, row_ordinal, bucket, {columns}
            ) FROM STDIN WITH (FORMAT text)"#
    )
}

trait SpillCopyRow {
    const RELATION: NativeGenerationSpillRelation;
    const OPERATION: &'static str;

    fn bucket_identity(&self) -> &str;
    fn encode_fields(&self, row: &mut TextRow);
}

impl SpillCopyRow for FileInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Files;
    const OPERATION: &'static str = "spill-copy-files";

    fn bucket_identity(&self) -> &str {
        self.file_id.as_str()
    }

    fn encode_fields(&self, row: &mut TextRow) {
        row.text(self.file_id.as_str());
        row.text(&self.normalized_path);
        row.text(&self.language);
        row.text(self.content_hash.as_str());
        row.number(self.byte_size);
        row.text(self.parse_status.as_str());
    }
}

impl SpillCopyRow for SymbolInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Symbols;
    const OPERATION: &'static str = "spill-copy-symbols";

    fn bucket_identity(&self) -> &str {
        self.symbol_id.as_str()
    }

    fn encode_fields(&self, row: &mut TextRow) {
        row.text(self.symbol_id.as_str());
        row.text(self.file_id.as_str());
        row.text(&self.symbol_kind);
        row.text(&self.qualified_name);
        row.text(&self.signature);
        row.number(self.start_byte);
        row.number(self.end_byte);
        row.number(self.start_line);
        row.number(self.end_line);
        row.text(self.structural_digest.as_str());
        row.optional_text(self.visibility.map(Visibility::as_str));
        row.number(self.export.exported);
        row.number(self.export.default_export);
        row.number(self.execution.async_symbol);
        row.number(self.execution.static_member);
        row.number(self.declaration_only);
        row.optional_number(
            self.betweenness_ppb
                .map(|score| f64::from(score) / 1_000_000_000.0),
        );
        row.optional_number(
            self.pagerank_ppb
                .map(|score| f64::from(score) / 1_000_000_000.0),
        );
    }
}

impl SpillCopyRow for EdgeInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Edges;
    const OPERATION: &'static str = "spill-copy-edges";

    fn bucket_identity(&self) -> &str {
        self.source_symbol_id.as_str()
    }

    fn encode_fields(&self, row: &mut TextRow) {
        row.text(self.source_symbol_id.as_str());
        row.text(self.target_symbol_id.as_str());
        row.text(self.kind.as_str());
        row.number(self.confidence);
        row.text(&self.provenance);
        row.number(self.site_count);
    }
}

impl SpillCopyRow for ReferenceInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::References;
    const OPERATION: &'static str = "spill-copy-references";

    fn bucket_identity(&self) -> &str {
        self.file_id.as_str()
    }

    fn encode_fields(&self, row: &mut TextRow) {
        row.text(self.file_id.as_str());
        row.optional_text(self.owner_symbol_id.as_ref().map(SymbolId::as_str));
        row.optional_text(self.target_symbol_id.as_ref().map(SymbolId::as_str));
        row.text(&self.reference_name);
        row.text(&self.reference_kind);
        row.number(self.start_byte);
        row.number(self.end_byte);
        row.number(self.confidence);
        row.text(&self.resolution_provenance);
        row.number(self.site_count);
        row.text(self.span_precision.as_str());
    }
}

impl SpillCopyRow for NumericalSiteInput {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::NumericalSites;
    const OPERATION: &'static str = "spill-copy-numerical-sites";

    fn bucket_identity(&self) -> &str {
        self.site_id.as_str()
    }

    fn encode_fields(&self, row: &mut TextRow) {
        row.text(self.site_id.as_str());
        row.text(self.file_id.as_str());
        row.optional_text(self.owner_symbol_id.as_ref().map(SymbolId::as_str));
        row.number(self.start_byte);
        row.number(self.end_byte);
        row.number(self.start_line);
        row.number(self.end_line);
        row.text(&self.operation);
        row.text(&self.hazard);
        row.text(&self.precision);
        row.text(self.expression_digest.as_str());
        row.number(self.confidence_ppm);
        row.text(&self.provenance);
        row.text(&self.evidence_level);
        row.text(&self.unknowns);
    }
}

impl SpillCopyRow for CanonicalSearchDocument {
    const RELATION: NativeGenerationSpillRelation = NativeGenerationSpillRelation::Documents;
    const OPERATION: &'static str = "spill-copy-documents";

    fn bucket_identity(&self) -> &str {
        self.document_id.as_str()
    }

    fn encode_fields(&self, row: &mut TextRow) {
        row.text(self.document_id.as_str());
        row.optional_text(self.file_id.as_ref().map(FileId::as_str));
        row.optional_text(self.symbol_id.as_ref().map(SymbolId::as_str));
        row.text(&self.path);
        row.text(&self.language);
        row.text(self.kind.as_str());
        row.text(&self.qualified_name);
        row.text(&self.code);
        row.text(&self.natural_text);
        row.text(&self.metadata_json);
        row.text(&self.metadata_json);
    }
}

/// Batches to copy and the accessor selecting one typed relation from each.
struct TypedFactCopy<'copy, T> {
    batches: &'copy [(i64, &'copy NativeGenerationSpillFactBatch)],
    select: for<'batch> fn(&'batch NativeGenerationSpillFactBatch) -> &'batch [T],
}

async fn copy_typed_fact_rows<T>(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    scope: SpillScope<'_>,
    request: TypedFactCopy<'_, T>,
) -> Result<(), StorageError>
where
    T: SpillCopyRow + Sync,
{
    let TypedFactCopy { batches, select } = request;
    let schema = scope.schema;
    let expected = batches.iter().try_fold(0_u64, |count, (_, batch)| {
        count.checked_add(usize_to_u64(select(batch).len()))
    });
    let expected = expected.ok_or(StorageError::InvalidInput {
        field: "spill_fact_batch_rows",
    })?;
    if expected == 0 {
        return Ok(());
    }
    let statement = typed_fact_insert_sql(schema, T::RELATION)?;
    let mut copy = CountedTextCopy::begin(
        transaction,
        CountedCopyRequest {
            statement: &statement,
            operation: T::OPERATION,
            expected_rows: expected,
        },
    )
    .await?;
    for (sequence, batch) in batches {
        for (ordinal, row) in select(batch).iter().enumerate() {
            copy.push(encode_spill_copy_row(
                scope,
                SpillRowPosition {
                    sequence: *sequence,
                    ordinal,
                },
                row,
            ))
            .await?;
        }
    }
    copy.finish().await
}

/// Exact batch sequence and row ordinal of one encoded spill row.
#[derive(Clone, Copy)]
struct SpillRowPosition {
    sequence: i64,
    ordinal: usize,
}

fn encode_spill_copy_row<T: SpillCopyRow>(
    scope: SpillScope<'_>,
    position: SpillRowPosition,
    value: &T,
) -> Result<Vec<u8>, StorageError> {
    let SpillRowPosition { sequence, ordinal } = position;
    let (project_id, generation_id) = (scope.project_id, scope.generation_id);
    let ordinal = i32::try_from(ordinal).map_err(|_| StorageError::InvalidInput {
        field: "spill_row_ordinal",
    })?;
    let bucket = spill_identity_bucket(value.bucket_identity())?;
    let mut row = TextRow::new();
    row.text(project_id.as_str());
    row.text(generation_id.as_str());
    row.number(sequence);
    row.number(ordinal);
    row.number(bucket);
    value.encode_fields(&mut row);
    Ok(row.finish())
}

fn spill_identity_bucket(identity: &str) -> Result<u16, StorageError> {
    identity
        .get(..3)
        .and_then(|prefix| u16::from_str_radix(prefix, 16).ok())
        .filter(|bucket| *bucket < CANONICAL_PARTITIONS * BUCKETS_PER_CANONICAL_PARTITION)
        .ok_or(StorageError::InvalidInput {
            field: "spill_bucket_identity",
        })
}

async fn insert_typed_fact_payloads(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    scope: SpillScope<'_>,
    payloads: TypedFactPayloads<'_>,
) -> Result<(), StorageError> {
    let TypedFactPayloads { relation, batches } = payloads;
    match relation {
        NativeGenerationSpillRelation::ExtractedFiles => Err(StorageError::InvalidInput {
            field: "spill_fact_relation",
        }),
        NativeGenerationSpillRelation::Files => {
            copy_typed_fact_rows(
                transaction,
                scope,
                TypedFactCopy {
                    batches,
                    select: |batch| batch.tables.files.as_slice(),
                },
            )
            .await
        }
        NativeGenerationSpillRelation::Symbols => {
            copy_typed_fact_rows(
                transaction,
                scope,
                TypedFactCopy {
                    batches,
                    select: |batch| batch.tables.symbols.as_slice(),
                },
            )
            .await
        }
        NativeGenerationSpillRelation::Edges => {
            copy_typed_fact_rows(
                transaction,
                scope,
                TypedFactCopy {
                    batches,
                    select: |batch| batch.tables.edges.as_slice(),
                },
            )
            .await
        }
        NativeGenerationSpillRelation::References => {
            copy_typed_fact_rows(
                transaction,
                scope,
                TypedFactCopy {
                    batches,
                    select: |batch| batch.tables.references.as_slice(),
                },
            )
            .await
        }
        NativeGenerationSpillRelation::NumericalSites => {
            copy_typed_fact_rows(
                transaction,
                scope,
                TypedFactCopy {
                    batches,
                    select: |batch| batch.tables.numerical_sites.as_slice(),
                },
            )
            .await
        }
        NativeGenerationSpillRelation::Documents => {
            copy_typed_fact_rows(
                transaction,
                scope,
                TypedFactCopy {
                    batches,
                    select: |batch| batch.tables.documents.as_slice(),
                },
            )
            .await
        }
    }
}

async fn stream_spilled_digest<Cancel, Progress, ProgressFuture>(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    scope: SpillScope<'_>,
    observation: DigestObservation<'_, Cancel, Progress>,
) -> Result<(ContentDigest, NativeGenerationSpillFactCounts), StorageError>
where
    Cancel: FnMut() -> bool,
    Progress: FnMut(u64) -> ProgressFuture,
    ProgressFuture: Future<Output = bool>,
{
    let DigestObservation {
        cancelled,
        progress,
    } = observation;
    let counts = canonical_fact_counts(transaction, scope).await?;
    let mut digest = LogicalDigestBuilder::new(GenerationDigestVersion::CURRENT);
    let mut stream = SpilledDigestStream {
        digest: &mut digest,
        cancelled,
        progress,
    };
    // Relation order is part of the digest and must match `encoded_relation_digests`.
    let begins: [fn(&mut LogicalDigestBuilder, u64); 5] = [
        LogicalDigestBuilder::begin_files,
        LogicalDigestBuilder::begin_symbols,
        LogicalDigestBuilder::begin_edges,
        LogicalDigestBuilder::begin_references,
        LogicalDigestBuilder::begin_numerical_sites,
    ];
    let totals = [
        counts.files,
        counts.symbols,
        counts.edges,
        counts.references,
        counts.numerical_sites,
    ];
    for ((begin, total), relation) in begins
        .into_iter()
        .zip(totals)
        .zip(encoded_relation_digests())
    {
        begin(stream.digest, total);
        digest_spilled_relation(transaction, &mut stream, (scope, relation)).await?;
    }
    stream.digest.begin_documents(counts.documents);
    digest_spilled_documents(transaction, &mut stream, scope).await?;
    Ok((digest.finish(), counts))
}

/// The six canonical relations counted for one generation, with the column each
/// count is read back through.
const COUNTED_RELATIONS: [(&str, &str); 6] = [
    ("files", "file_count"),
    ("symbols", "symbol_count"),
    ("edges", "edge_count"),
    ("references", "reference_count"),
    ("numerical_sites", "numerical_site_count"),
    ("documents", "document_count"),
];

/// Count every canonical relation for one generation under a table prefix.
///
/// The spilled tables and the published tables hold the same six relations, so
/// the projection is built once rather than copied per caller.
fn fact_count_sql(schema: &str, prefix: &str) -> String {
    let projection = COUNTED_RELATIONS
        .iter()
        .map(|(relation, column)| {
            let table = if prefix.is_empty() {
                match *relation {
                    "documents" => "search_documents".to_owned(),
                    other => other.to_owned(),
                }
            } else {
                format!("{prefix}{relation}")
            };
            format!(
                r#"(SELECT count(*) FROM {schema}."{table}"
                  WHERE project_id = CAST($1 AS uuid)
                    AND generation_id = CAST($2 AS uuid)) AS {column}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",\n            ");
    format!("SELECT\n            {projection}")
}

fn decode_fact_counts(
    row: &sqlx_postgres::PgRow,
) -> Result<NativeGenerationSpillFactCounts, StorageError> {
    Ok(NativeGenerationSpillFactCounts {
        files: read_u64(row, "file_count", "spill_file_count")?,
        symbols: read_u64(row, "symbol_count", "spill_symbol_count")?,
        edges: read_u64(row, "edge_count", "spill_edge_count")?,
        references: read_u64(row, "reference_count", "spill_reference_count")?,
        numerical_sites: read_u64(row, "numerical_site_count", "spill_numerical_count")?,
        documents: read_u64(row, "document_count", "spill_document_count")?,
    })
}

pub(crate) async fn canonical_fact_counts(
    connection: &mut sqlx_postgres::PgConnection,
    scope: SpillScope<'_>,
) -> Result<NativeGenerationSpillFactCounts, StorageError> {
    let SpillScope {
        schema,
        project_id,
        generation_id,
    } = scope;
    let sql = fact_count_sql(schema, "");
    let row = audited_query(sql)
        .bind(project_id.as_str())
        .bind(generation_id.as_str())
        .fetch_one(connection)
        .await
        .map_err(|error| classify_statement_error(&error, "spill-read-canonical-counts"))?;
    decode_fact_counts(&row)
}

struct DigestPulse<'a, Cancel, Progress> {
    cancelled: &'a mut Cancel,
    progress: &'a mut Progress,
    pending: u64,
}

impl<'a, Cancel, Progress> DigestPulse<'a, Cancel, Progress>
where
    Cancel: FnMut() -> bool,
{
    const fn new(cancelled: &'a mut Cancel, progress: &'a mut Progress) -> Self {
        Self {
            cancelled,
            progress,
            pending: 0,
        }
    }

    async fn row<ProgressFuture>(&mut self) -> Result<(), StorageError>
    where
        Progress: FnMut(u64) -> ProgressFuture,
        ProgressFuture: Future<Output = bool>,
    {
        if (self.cancelled)() {
            return Err(StorageError::GenerationSpillCancelled);
        }
        self.pending = self.pending.saturating_add(1);
        if self.pending >= 1_024 {
            if !(self.progress)(self.pending).await {
                return Err(StorageError::GenerationSpillCancelled);
            }
            self.pending = 0;
        }
        Ok(())
    }

    async fn finish<ProgressFuture>(self) -> Result<(), StorageError>
    where
        Progress: FnMut(u64) -> ProgressFuture,
        ProgressFuture: Future<Output = bool>,
    {
        if self.pending > 0 && !(self.progress)(self.pending).await {
            return Err(StorageError::GenerationSpillCancelled);
        }
        Ok(())
    }
}

fn digest_text_sql(expression: &str) -> String {
    format!(
        "convert_to(octet_length(({expression})::text)::text, 'UTF8') \
         || decode('00', 'hex') || convert_to(({expression})::text, 'UTF8')"
    )
}

fn digest_optional_text_sql(expression: &str) -> String {
    let encoded = digest_text_sql(expression);
    format!(
        "CASE WHEN ({expression}) IS NULL THEN decode('00', 'hex') \
         ELSE decode('01', 'hex') || {encoded} END"
    )
}

fn digest_boolean_sql(expression: &str) -> String {
    format!("decode(CASE WHEN ({expression}) THEN '01' ELSE '00' END, 'hex')")
}

fn digest_relation_sql(schema: &str, relation: &SpilledRelationDigest) -> String {
    let SpilledRelationDigest {
        relation: table,
        columns: fields,
        order_key: order,
        ..
    } = relation;
    let encoded = fields.join(" || ");
    format!(
        r#"SELECT {encoded} AS digest_row
            FROM {schema}."{table}"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
            ORDER BY {order}"#
    )
}

struct DigestEncodedRequest<'a> {
    project_id: &'a ProjectId,
    generation_id: &'a GenerationId,
    sql: String,
    operation: &'static str,
}

async fn digest_encoded_relation<Cancel, Progress, ProgressFuture>(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    stream: &mut SpilledDigestStream<'_, Cancel, Progress>,
    request: DigestEncodedRequest<'_>,
) -> Result<(), StorageError>
where
    Cancel: FnMut() -> bool,
    Progress: FnMut(u64) -> ProgressFuture,
    ProgressFuture: Future<Output = bool>,
{
    let mut rows = audited_query(request.sql)
        .bind(request.project_id.as_str())
        .bind(request.generation_id.as_str())
        .fetch(&mut **transaction);
    let mut pulse = DigestPulse::new(stream.cancelled, stream.progress);
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|error| classify_statement_error(&error, request.operation))?
    {
        pulse.row().await?;
        let encoded = row
            .try_get::<Vec<u8>, _>(0)
            .map_err(|_| corrupt("digest_row"))?;
        stream.digest.push_encoded_row(&encoded);
    }
    pulse.finish().await
}

/// One canonical relation in the exact order the logical digest consumes it.
///
/// The five encoded relations differ only by relation name, encoded columns, and
/// order key, so they are described rather than copied. The descriptions and
/// their order are part of the generation digest and must not be reordered.
struct SpilledRelationDigest {
    relation: &'static str,
    columns: Vec<String>,
    order_key: &'static str,
    operation: &'static str,
}

/// Cancellation and progress channel for one streaming digest pass.
struct DigestObservation<'observation, Cancel, Progress> {
    cancelled: &'observation mut Cancel,
    progress: &'observation mut Progress,
}

/// Mutable digest sinks threaded through one streaming pass.
struct SpilledDigestStream<'stream, Cancel, Progress> {
    digest: &'stream mut LogicalDigestBuilder,
    cancelled: &'stream mut Cancel,
    progress: &'stream mut Progress,
}

/// Exact schema and project/generation fence for one spilled operation.
#[derive(Clone, Copy)]
pub(crate) struct SpillScope<'scope> {
    pub(crate) schema: &'scope str,
    pub(crate) project_id: &'scope ProjectId,
    pub(crate) generation_id: &'scope GenerationId,
}

fn encoded_relation_digests() -> [SpilledRelationDigest; 5] {
    [
        SpilledRelationDigest {
            relation: "files",
            columns: vec![
                digest_text_sql("file_id::text"),
                digest_text_sql("normalized_path"),
                digest_text_sql("language"),
                digest_text_sql("content_hash"),
                "int8send(byte_size)".to_owned(),
                digest_text_sql("parse_status"),
            ],
            order_key: "file_id",
            operation: "spill-digest-files",
        },
        SpilledRelationDigest {
            relation: "symbols",
            columns: vec![
                digest_text_sql("symbol_id::text"),
                digest_text_sql("file_id::text"),
                digest_text_sql("symbol_kind"),
                digest_text_sql("qualified_name"),
                digest_text_sql("signature"),
                "int8send(start_byte)".to_owned(),
                "int8send(end_byte)".to_owned(),
                "int4send(start_line)".to_owned(),
                "int4send(end_line)".to_owned(),
                digest_text_sql("structural_digest"),
                digest_optional_text_sql("visibility"),
                digest_boolean_sql("exported"),
                digest_boolean_sql("default_export"),
                digest_boolean_sql("async_symbol"),
                digest_boolean_sql("static_member"),
                digest_boolean_sql("declaration_only"),
            ],
            order_key: "symbol_id",
            operation: "spill-digest-symbols",
        },
        SpilledRelationDigest {
            relation: "edges",
            columns: vec![
                digest_text_sql("source_symbol_id::text"),
                digest_text_sql("target_symbol_id::text"),
                digest_text_sql("edge_kind"),
                "float4send(confidence)".to_owned(),
                digest_text_sql("provenance"),
                "int4send(site_count::integer)".to_owned(),
            ],
            order_key: "source_symbol_id, target_symbol_id, edge_kind COLLATE \"C\", provenance COLLATE \"C\"",
            operation: "spill-digest-edges",
        },
        SpilledRelationDigest {
            relation: "references",
            columns: vec![
                digest_text_sql("file_id::text"),
                digest_optional_text_sql("owner_symbol_id::text"),
                digest_optional_text_sql("target_symbol_id::text"),
                digest_text_sql("reference_name"),
                digest_text_sql("reference_kind"),
                "int8send(start_byte)".to_owned(),
                "int8send(end_byte)".to_owned(),
                "float4send(confidence)".to_owned(),
                digest_text_sql("resolution_provenance"),
                "int4send(site_count::integer)".to_owned(),
                digest_text_sql("span_precision"),
            ],
            order_key: "file_id, owner_symbol_id NULLS FIRST, target_symbol_id NULLS FIRST, reference_name COLLATE \"C\", reference_kind COLLATE \"C\", start_byte, end_byte, resolution_provenance COLLATE \"C\"",
            operation: "spill-digest-references",
        },
        SpilledRelationDigest {
            relation: "numerical_sites",
            columns: vec![
                digest_text_sql("numerical_site_id::text"),
                digest_text_sql("file_id::text"),
                digest_optional_text_sql("owner_symbol_id::text"),
                "int8send(start_byte)".to_owned(),
                "int8send(end_byte)".to_owned(),
                "int4send(start_line)".to_owned(),
                "int4send(end_line)".to_owned(),
                digest_text_sql("operation"),
                digest_text_sql("hazard"),
                digest_text_sql("precision"),
                digest_text_sql("expression_digest"),
                "int4send(confidence_ppm)".to_owned(),
                digest_text_sql("provenance"),
                digest_text_sql("evidence_level"),
                digest_text_sql("unknowns"),
            ],
            order_key: "numerical_site_id",
            operation: "spill-digest-numerical-sites",
        },
    ]
}

async fn digest_spilled_relation<Cancel, Progress, ProgressFuture>(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    stream: &mut SpilledDigestStream<'_, Cancel, Progress>,
    request: (SpillScope<'_>, SpilledRelationDigest),
) -> Result<(), StorageError>
where
    Cancel: FnMut() -> bool,
    Progress: FnMut(u64) -> ProgressFuture,
    ProgressFuture: Future<Output = bool>,
{
    let (scope, relation) = request;
    let sql = digest_relation_sql(scope.schema, &relation);
    digest_encoded_relation(
        transaction,
        stream,
        DigestEncodedRequest {
            project_id: scope.project_id,
            generation_id: scope.generation_id,
            sql,
            operation: relation.operation,
        },
    )
    .await
}

async fn digest_spilled_documents<Cancel, Progress, ProgressFuture>(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    stream: &mut SpilledDigestStream<'_, Cancel, Progress>,
    scope: SpillScope<'_>,
) -> Result<(), StorageError>
where
    Cancel: FnMut() -> bool,
    Progress: FnMut(u64) -> ProgressFuture,
    ProgressFuture: Future<Output = bool>,
{
    let SpillScope {
        schema,
        project_id,
        generation_id,
    } = scope;
    let missing_sql = format!(
        r#"SELECT 1 FROM {schema}."search_documents"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
              AND metadata_json IS NULL
            LIMIT 1"#
    );
    let missing = audited_query(missing_sql)
        .bind(project_id.as_str())
        .bind(generation_id.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|error| classify_statement_error(&error, "spill-digest-document-metadata"))?;
    if missing.is_none() {
        let sql = digest_relation_sql(
            schema,
            &SpilledRelationDigest {
                relation: "search_documents",
                columns: vec![
                    digest_text_sql("document_id::text"),
                    digest_optional_text_sql("file_id::text"),
                    digest_optional_text_sql("symbol_id::text"),
                    digest_text_sql("path"),
                    digest_text_sql("language"),
                    digest_text_sql("document_kind"),
                    digest_text_sql("qualified_name"),
                    digest_text_sql("code"),
                    digest_text_sql("natural_text"),
                    digest_text_sql("metadata_json"),
                ],
                order_key: "document_id",
                operation: "spill-digest-documents",
            },
        );
        return digest_encoded_relation(
            transaction,
            stream,
            DigestEncodedRequest {
                project_id,
                generation_id,
                sql,
                operation: "spill-digest-documents",
            },
        )
        .await;
    }
    let sql = format!(
        r#"SELECT document_id::text, file_id::text, symbol_id::text, path,
                  language, document_kind, qualified_name, code, natural_text, metadata::text
            FROM {schema}."search_documents"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)
            ORDER BY document_id"#
    );
    let mut rows = audited_query(sql)
        .bind(project_id.as_str())
        .bind(generation_id.as_str())
        .fetch(&mut **transaction);
    let mut pulse = DigestPulse::new(stream.cancelled, stream.progress);
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|error| classify_statement_error(&error, "spill-digest-documents"))?
    {
        pulse.row().await?;
        let raw_kind = read_string(&row, "document_kind")?;
        let raw_metadata = read_string(&row, "metadata")?;
        let metadata = serde_json::from_str(&raw_metadata).map_err(|_| corrupt("metadata"))?;
        let metadata_json =
            canonical_stored_metadata(&metadata).map_err(|_| corrupt("metadata"))?;
        stream.digest.push_document(&CanonicalSearchDocument {
            document_id: parse_document_id(&row, "document_id")?,
            file_id: parse_optional_file_id(&row, "file_id")?,
            symbol_id: parse_optional_symbol_id(&row, "symbol_id")?,
            path: read_string(&row, "path")?,
            language: read_string(&row, "language")?,
            kind: parse_document_kind(&raw_kind)?,
            qualified_name: read_string(&row, "qualified_name")?,
            code: read_string(&row, "code")?,
            natural_text: read_string(&row, "natural_text")?,
            metadata_json,
        });
    }
    pulse.finish().await?;
    Ok(())
}

fn read_string(row: &sqlx_postgres::PgRow, column: &'static str) -> Result<String, StorageError> {
    row.try_get::<String, _>(column)
        .map_err(|_| corrupt(column))
}

fn parse_optional_file_id(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
) -> Result<Option<FileId>, StorageError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|_| corrupt("file_id"))?
        .map(|raw| FileId::parse(&raw).map_err(|_| corrupt("file_id")))
        .transpose()
}

fn parse_optional_symbol_id(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
) -> Result<Option<SymbolId>, StorageError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|_| corrupt(column))?
        .map(|raw| SymbolId::parse(&raw).map_err(|_| corrupt(column)))
        .transpose()
}

fn parse_document_id(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
) -> Result<DocumentId, StorageError> {
    let raw = read_string(row, column)?;
    DocumentId::parse(&raw).map_err(|_| corrupt("document_id"))
}

fn parse_document_kind(raw: &str) -> Result<DocumentKind, StorageError> {
    match raw {
        "symbol" => Ok(DocumentKind::Symbol),
        "file" => Ok(DocumentKind::File),
        "documentation" => Ok(DocumentKind::Documentation),
        "test" => Ok(DocumentKind::Test),
        "configuration" => Ok(DocumentKind::Configuration),
        _ => Err(corrupt("document_kind")),
    }
}

struct CanonicalPartitionSql {
    conflict: String,
    insert: String,
    raw_table: &'static str,
}

fn canonical_partition_sql(
    schema: &str,
    relation: NativeGenerationSpillRelation,
) -> CanonicalPartitionSql {
    match relation {
        NativeGenerationSpillRelation::ExtractedFiles => CanonicalPartitionSql {
            conflict: "SELECT 1 WHERE false".to_owned(),
            insert: "SELECT 1 WHERE false".to_owned(),
            raw_table: "native_generation_spill_rows",
        },
        NativeGenerationSpillRelation::Files => canonical_file_partition_sql(schema),
        NativeGenerationSpillRelation::Symbols => canonical_symbol_partition_sql(schema),
        NativeGenerationSpillRelation::Edges => canonical_edge_partition_sql(schema),
        NativeGenerationSpillRelation::References => canonical_reference_partition_sql(schema),
        NativeGenerationSpillRelation::NumericalSites => canonical_numerical_partition_sql(schema),
        NativeGenerationSpillRelation::Documents => canonical_document_partition_sql(schema),
    }
}

fn canonical_file_partition_sql(schema: &str) -> CanonicalPartitionSql {
    CanonicalPartitionSql {
        conflict: format!(
            r#"WITH input AS MATERIALIZED (
                    SELECT * FROM {schema}."native_generation_spill_files"
                    WHERE project_id = CAST($1 AS uuid)
                      AND generation_id = CAST($2 AS uuid)
                      AND bucket BETWEEN $3 AND $4
                ), conflicts AS (
                    SELECT file_id FROM input GROUP BY file_id
                    HAVING count(DISTINCT ROW(
                        normalized_path, language, content_hash, byte_size, parse_status
                    )) > 1
                    UNION ALL
                    SELECT NULL::uuid FROM input GROUP BY normalized_path
                    HAVING count(DISTINCT file_id) > 1
                ) SELECT 1 FROM conflicts LIMIT 1"#
        ),
        insert: format!(
            r#"INSERT INTO {schema}."files" (
                    project_id, generation_id, file_id, normalized_path,
                    language, content_hash, byte_size, parse_status
                )
                SELECT DISTINCT ON (file_id)
                       project_id, generation_id, file_id, normalized_path,
                       language, content_hash, byte_size, parse_status
                FROM {schema}."native_generation_spill_files"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND bucket BETWEEN $3 AND $4
                ORDER BY file_id, batch_sequence, row_ordinal"#
        ),
        raw_table: "native_generation_spill_files",
    }
}

fn canonical_symbol_partition_sql(schema: &str) -> CanonicalPartitionSql {
    CanonicalPartitionSql {
        conflict: format!(
            r#"WITH input AS MATERIALIZED (
                    SELECT * FROM {schema}."native_generation_spill_symbols"
                    WHERE project_id = CAST($1 AS uuid)
                      AND generation_id = CAST($2 AS uuid)
                      AND bucket BETWEEN $3 AND $4
                ), invalid AS (
                    SELECT 1 FROM input AS symbols
                    LEFT JOIN {schema}."files" AS files
                      ON files.project_id = symbols.project_id
                     AND files.generation_id = symbols.generation_id
                     AND files.file_id = symbols.file_id
                    WHERE files.file_id IS NULL
                       OR files.parse_status NOT IN ('parsed', 'partial')
                       OR symbols.end_byte > files.byte_size
                ), conflicts AS (
                    SELECT 1 FROM input GROUP BY symbol_id
                    HAVING count(DISTINCT ROW(
                        file_id, symbol_kind, qualified_name, signature,
                        start_byte, end_byte, start_line, end_line, structural_digest,
                        visibility, exported, default_export, async_symbol,
                        static_member, declaration_only, betweenness, pagerank
                    )) > 1
                )
                SELECT 1 FROM (
                    SELECT 1 FROM invalid
                    UNION ALL
                    SELECT 1 FROM conflicts
                ) AS rejected LIMIT 1"#
        ),
        insert: format!(
            r#"INSERT INTO {schema}."symbols" (
                    project_id, generation_id, symbol_id, file_id, symbol_kind,
                    qualified_name, signature, start_byte, end_byte, start_line,
                    end_line, structural_digest, visibility, exported, default_export,
                    async_symbol, static_member, declaration_only, betweenness, pagerank
                )
                SELECT DISTINCT ON (symbol_id)
                       project_id, generation_id, symbol_id, file_id, symbol_kind,
                       qualified_name, signature, start_byte, end_byte, start_line,
                       end_line, structural_digest, visibility, exported, default_export,
                       async_symbol, static_member, declaration_only, betweenness, pagerank
                FROM {schema}."native_generation_spill_symbols"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND bucket BETWEEN $3 AND $4
                ORDER BY symbol_id, batch_sequence, row_ordinal"#
        ),
        raw_table: "native_generation_spill_symbols",
    }
}

fn canonical_edge_partition_sql(schema: &str) -> CanonicalPartitionSql {
    CanonicalPartitionSql {
        conflict: format!(
            r#"WITH input AS MATERIALIZED (
                    SELECT * FROM {schema}."native_generation_spill_edges"
                    WHERE project_id = CAST($1 AS uuid)
                      AND generation_id = CAST($2 AS uuid)
                      AND bucket BETWEEN $3 AND $4
                ), invalid AS (
                    SELECT 1 FROM input AS edges
                    LEFT JOIN {schema}."symbols" AS sources
                      ON sources.project_id = edges.project_id
                     AND sources.generation_id = edges.generation_id
                     AND sources.symbol_id = edges.source_symbol_id
                    LEFT JOIN {schema}."symbols" AS targets
                      ON targets.project_id = edges.project_id
                     AND targets.generation_id = edges.generation_id
                     AND targets.symbol_id = edges.target_symbol_id
                    WHERE sources.symbol_id IS NULL OR targets.symbol_id IS NULL
                ), conflicts AS (
                    SELECT 1 FROM input
                    GROUP BY source_symbol_id, target_symbol_id, edge_kind, provenance
                    HAVING sum(site_count) > 100000000
                )
                SELECT 1 FROM (
                    SELECT 1 FROM invalid
                    UNION ALL
                    SELECT 1 FROM conflicts
                ) AS rejected LIMIT 1"#
        ),
        insert: format!(
            r#"INSERT INTO {schema}."edges" (
                    project_id, generation_id, source_symbol_id, target_symbol_id,
                    edge_kind, confidence, provenance, site_count
                )
                SELECT project_id, generation_id, source_symbol_id, target_symbol_id,
                       edge_kind, max(confidence), provenance, sum(site_count)
                FROM {schema}."native_generation_spill_edges"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND bucket BETWEEN $3 AND $4
                GROUP BY project_id, generation_id, source_symbol_id,
                         target_symbol_id, edge_kind, provenance
                ORDER BY source_symbol_id, target_symbol_id, edge_kind, provenance"#
        ),
        raw_table: "native_generation_spill_edges",
    }
}

fn canonical_reference_partition_sql(schema: &str) -> CanonicalPartitionSql {
    CanonicalPartitionSql {
        conflict: format!(
            r#"WITH input AS MATERIALIZED (
                    SELECT * FROM {schema}."native_generation_spill_references"
                    WHERE project_id = CAST($1 AS uuid)
                      AND generation_id = CAST($2 AS uuid)
                      AND bucket BETWEEN $3 AND $4
                ), invalid AS (
                    SELECT 1 FROM input AS refs
                    LEFT JOIN {schema}."files" AS files
                      ON files.project_id = refs.project_id
                     AND files.generation_id = refs.generation_id
                     AND files.file_id = refs.file_id
                    LEFT JOIN {schema}."symbols" AS owners
                      ON owners.project_id = refs.project_id
                     AND owners.generation_id = refs.generation_id
                     AND owners.symbol_id = refs.owner_symbol_id
                    LEFT JOIN {schema}."symbols" AS targets
                      ON targets.project_id = refs.project_id
                     AND targets.generation_id = refs.generation_id
                     AND targets.symbol_id = refs.target_symbol_id
                    WHERE files.file_id IS NULL
                       OR files.parse_status NOT IN ('parsed', 'partial')
                       OR refs.end_byte > files.byte_size
                       OR (refs.owner_symbol_id IS NOT NULL AND owners.symbol_id IS NULL)
                       OR (owners.symbol_id IS NOT NULL AND owners.file_id <> refs.file_id)
                       OR (refs.target_symbol_id IS NOT NULL AND targets.symbol_id IS NULL)
                ), conflicts AS (
                    SELECT 1 FROM input
                    GROUP BY file_id, owner_symbol_id, target_symbol_id, reference_name,
                             reference_kind, start_byte, end_byte, resolution_provenance
                    HAVING count(DISTINCT ROW(confidence, site_count, span_precision)) > 1
                )
                SELECT 1 FROM (
                    SELECT 1 FROM invalid
                    UNION ALL
                    SELECT 1 FROM conflicts
                ) AS rejected LIMIT 1"#
        ),
        insert: format!(
            r#"INSERT INTO {schema}."references" (
                    project_id, generation_id, file_id, owner_symbol_id,
                    target_symbol_id, reference_name, reference_kind, start_byte,
                    end_byte, confidence, resolution_provenance, site_count, span_precision
                )
                SELECT DISTINCT ON (
                           file_id, owner_symbol_id, target_symbol_id, reference_name,
                           reference_kind, start_byte, end_byte, resolution_provenance
                       )
                       project_id, generation_id, file_id, owner_symbol_id,
                       target_symbol_id, reference_name, reference_kind, start_byte,
                       end_byte, confidence, resolution_provenance, site_count, span_precision
                FROM {schema}."native_generation_spill_references"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND bucket BETWEEN $3 AND $4
                ORDER BY file_id, owner_symbol_id, target_symbol_id, reference_name,
                         reference_kind, start_byte, end_byte, resolution_provenance,
                         batch_sequence, row_ordinal"#
        ),
        raw_table: "native_generation_spill_references",
    }
}

fn canonical_numerical_partition_sql(schema: &str) -> CanonicalPartitionSql {
    CanonicalPartitionSql {
        conflict: format!(
            r#"WITH input AS MATERIALIZED (
                    SELECT * FROM {schema}."native_generation_spill_numerical_sites"
                    WHERE project_id = CAST($1 AS uuid)
                      AND generation_id = CAST($2 AS uuid)
                      AND bucket BETWEEN $3 AND $4
                ), invalid AS (
                    SELECT 1 FROM input AS sites
                    LEFT JOIN {schema}."files" AS files
                      ON files.project_id = sites.project_id
                     AND files.generation_id = sites.generation_id
                     AND files.file_id = sites.file_id
                    LEFT JOIN {schema}."symbols" AS owners
                      ON owners.project_id = sites.project_id
                     AND owners.generation_id = sites.generation_id
                     AND owners.symbol_id = sites.owner_symbol_id
                    WHERE files.file_id IS NULL
                       OR files.parse_status NOT IN ('parsed', 'partial')
                       OR sites.end_byte > files.byte_size
                       OR (sites.owner_symbol_id IS NOT NULL AND owners.symbol_id IS NULL)
                       OR (
                           owners.symbol_id IS NOT NULL
                           AND (
                               owners.file_id <> sites.file_id
                               OR sites.start_byte < owners.start_byte
                               OR sites.end_byte > owners.end_byte
                           )
                       )
                ), conflicts AS (
                    SELECT 1 FROM input GROUP BY numerical_site_id
                    HAVING count(DISTINCT ROW(
                        file_id, owner_symbol_id, start_byte, end_byte, start_line,
                        end_line, operation, hazard, precision, expression_digest,
                        confidence_ppm, provenance, evidence_level, unknowns
                    )) > 1
                )
                SELECT 1 FROM (
                    SELECT 1 FROM invalid
                    UNION ALL
                    SELECT 1 FROM conflicts
                ) AS rejected LIMIT 1"#
        ),
        insert: format!(
            r#"INSERT INTO {schema}."numerical_sites" (
                    project_id, generation_id, numerical_site_id, file_id,
                    owner_symbol_id, start_byte, end_byte, start_line, end_line,
                    operation, hazard, precision, expression_digest, confidence_ppm,
                    provenance, evidence_level, unknowns
                )
                SELECT DISTINCT ON (numerical_site_id)
                       project_id, generation_id, numerical_site_id, file_id,
                       owner_symbol_id, start_byte, end_byte, start_line, end_line,
                       operation, hazard, precision, expression_digest, confidence_ppm,
                       provenance, evidence_level, unknowns
                FROM {schema}."native_generation_spill_numerical_sites"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND bucket BETWEEN $3 AND $4
                ORDER BY numerical_site_id, batch_sequence, row_ordinal"#
        ),
        raw_table: "native_generation_spill_numerical_sites",
    }
}

fn canonical_document_partition_sql(schema: &str) -> CanonicalPartitionSql {
    CanonicalPartitionSql {
        conflict: format!(
            r#"WITH invalid AS (
                    SELECT 1 FROM {schema}."native_generation_spill_documents" AS documents
                    LEFT JOIN {schema}."files" AS explicit_files
                      ON explicit_files.project_id = documents.project_id
                     AND explicit_files.generation_id = documents.generation_id
                     AND explicit_files.file_id = documents.file_id
                    LEFT JOIN {schema}."symbols" AS symbols
                      ON symbols.project_id = documents.project_id
                     AND symbols.generation_id = documents.generation_id
                     AND symbols.symbol_id = documents.symbol_id
                    LEFT JOIN {schema}."files" AS symbol_files
                      ON symbol_files.project_id = symbols.project_id
                     AND symbol_files.generation_id = symbols.generation_id
                     AND symbol_files.file_id = symbols.file_id
                    WHERE documents.project_id = CAST($1 AS uuid)
                      AND documents.generation_id = CAST($2 AS uuid)
                      AND documents.bucket BETWEEN $3 AND $4
                      AND (
                           (documents.file_id IS NOT NULL AND explicit_files.file_id IS NULL)
                           OR (documents.symbol_id IS NOT NULL AND symbols.symbol_id IS NULL)
                           OR (symbols.symbol_id IS NOT NULL AND symbol_files.file_id IS NULL)
                           OR (
                               explicit_files.file_id IS NOT NULL
                               AND symbol_files.file_id IS NOT NULL
                               AND explicit_files.file_id <> symbol_files.file_id
                           )
                           OR (
                               COALESCE(explicit_files.file_id, symbol_files.file_id) IS NOT NULL
                               AND documents.path <> COALESCE(
                                   explicit_files.normalized_path, symbol_files.normalized_path
                               )
                           )
                           OR (
                               COALESCE(explicit_files.file_id, symbol_files.file_id) IS NOT NULL
                               AND documents.language <> COALESCE(
                                   explicit_files.language, symbol_files.language
                               )
                           )
                      )
                ), conflicts AS (
                    SELECT 1
                    FROM {schema}."native_generation_spill_documents" AS documents
                    JOIN {schema}."native_generation_spill_documents" AS duplicate
                      ON duplicate.project_id = documents.project_id
                     AND duplicate.generation_id = documents.generation_id
                     AND duplicate.bucket = documents.bucket
                     AND duplicate.document_id = documents.document_id
                     AND (duplicate.batch_sequence, duplicate.row_ordinal)
                         > (documents.batch_sequence, documents.row_ordinal)
                    WHERE documents.project_id = CAST($1 AS uuid)
                      AND documents.generation_id = CAST($2 AS uuid)
                      AND documents.bucket BETWEEN $3 AND $4
                      AND ROW(
                          documents.file_id, documents.symbol_id, documents.path,
                          documents.language, documents.document_kind,
                          documents.qualified_name, documents.code,
                          documents.natural_text, documents.metadata_json
                      ) IS DISTINCT FROM ROW(
                          duplicate.file_id, duplicate.symbol_id, duplicate.path,
                          duplicate.language, duplicate.document_kind,
                          duplicate.qualified_name, duplicate.code,
                          duplicate.natural_text, duplicate.metadata_json
                      )
                )
                SELECT 1 FROM (
                    SELECT 1 FROM invalid
                    UNION ALL
                    SELECT 1 FROM conflicts
                ) AS rejected LIMIT 1"#
        ),
        insert: format!(
            r#"INSERT INTO {schema}."search_documents" (
                    project_id, generation_id, document_id, file_id, symbol_id,
                    path, language, document_kind, qualified_name, code,
                    natural_text, metadata, metadata_json
                )
                SELECT DISTINCT ON (document_id)
                       project_id, generation_id, document_id, file_id, symbol_id,
                       path, language, document_kind, qualified_name, code,
                       natural_text, metadata, metadata_json
                FROM {schema}."native_generation_spill_documents"
                WHERE project_id = CAST($1 AS uuid)
                  AND generation_id = CAST($2 AS uuid)
                  AND bucket BETWEEN $3 AND $4
                ORDER BY document_id, batch_sequence, row_ordinal"#
        ),
        raw_table: "native_generation_spill_documents",
    }
}

fn parse_fact_relation(raw: &str) -> Result<NativeGenerationSpillRelation, StorageError> {
    match raw {
        "files" => Ok(NativeGenerationSpillRelation::Files),
        "symbols" => Ok(NativeGenerationSpillRelation::Symbols),
        "edges" => Ok(NativeGenerationSpillRelation::Edges),
        "references" => Ok(NativeGenerationSpillRelation::References),
        "numerical_sites" => Ok(NativeGenerationSpillRelation::NumericalSites),
        "documents" => Ok(NativeGenerationSpillRelation::Documents),
        _ => Err(corrupt("spill_canonical_relation")),
    }
}

const fn next_fact_relation(
    relation: NativeGenerationSpillRelation,
) -> Option<NativeGenerationSpillRelation> {
    match relation {
        NativeGenerationSpillRelation::ExtractedFiles => Some(NativeGenerationSpillRelation::Files),
        NativeGenerationSpillRelation::Files => Some(NativeGenerationSpillRelation::Symbols),
        NativeGenerationSpillRelation::Symbols => Some(NativeGenerationSpillRelation::Edges),
        NativeGenerationSpillRelation::Edges => Some(NativeGenerationSpillRelation::References),
        NativeGenerationSpillRelation::References => {
            Some(NativeGenerationSpillRelation::NumericalSites)
        }
        NativeGenerationSpillRelation::NumericalSites => {
            Some(NativeGenerationSpillRelation::Documents)
        }
        NativeGenerationSpillRelation::Documents => None,
    }
}

fn read_u64<Index>(
    row: &sqlx_postgres::PgRow,
    index: Index,
    field: &'static str,
) -> Result<u64, StorageError>
where
    Index: sqlx_core::column::ColumnIndex<sqlx_postgres::PgRow>,
{
    row.try_get::<i64, _>(index)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| corrupt(field))
}

fn validate_fact_append_group(
    batches: &[NativeGenerationSpillFactBatch],
) -> Result<(), StorageError> {
    if batches.is_empty() || batches.len() > MAXIMUM_FACT_APPEND_BATCHES {
        return Err(StorageError::InvalidInput {
            field: "spill_fact_batches",
        });
    }
    let mut sequences = BTreeSet::new();
    let mut logical_bytes = 0_u64;
    let mut row_count = 0_u64;
    for batch in batches {
        as_i64(batch.sequence, "spill_batch_sequence")?;
        if !sequences.insert(batch.sequence) {
            return Err(StorageError::InvalidInput {
                field: "spill_batch_sequence",
            });
        }
        logical_bytes = logical_bytes
            .checked_add(batch.logical_bytes)
            .filter(|bytes| *bytes <= MAXIMUM_FACT_APPEND_BYTES)
            .ok_or(StorageError::InvalidInput {
                field: "spill_fact_batch_bytes",
            })?;
        row_count = row_count
            .checked_add(batch.row_count)
            .filter(|rows| *rows <= MAXIMUM_FACT_APPEND_ROWS)
            .ok_or(StorageError::InvalidInput {
                field: "spill_fact_batch_rows",
            })?;
    }
    Ok(())
}

fn stored_fact_batch_is_exact(
    existing: Option<&BTreeMap<String, StoredFactPayload>>,
    batch: &NativeGenerationSpillFactBatch,
) -> Result<bool, StorageError> {
    let Some(existing) = existing else {
        return Ok(false);
    };
    let exact = existing.len() == batch.payloads.len()
        && batch.payloads.iter().all(|payload| {
            existing
                .get(payload.relation.as_str())
                .is_some_and(|stored| {
                    usize::try_from(stored.row_count).ok() == Some(payload.row_count)
                        && u64::try_from(stored.logical_bytes).ok() == Some(payload.logical_bytes)
                        && stored.digest == payload.digest.as_str()
                })
        });
    if exact {
        Ok(true)
    } else {
        Err(StorageError::GenerationSpillConflict)
    }
}

fn extracted_row_arrays(
    batch: &NativeGenerationSpillExtractedBatch,
) -> Result<ExtractedRowArrays, StorageError> {
    let capacity = batch.rows.len();
    let mut arrays = ExtractedRowArrays {
        inline: InlineExtractedArrays {
            ordinals: Vec::with_capacity(capacity),
            sort_keys: Vec::with_capacity(capacity),
            payloads: Vec::with_capacity(capacity),
            payload_digests: Vec::with_capacity(capacity),
        },
        cached: CachedExtractedArrays {
            ordinals: Vec::with_capacity(capacity),
            sort_keys: Vec::with_capacity(capacity),
            contracts: Vec::with_capacity(capacity),
            path_digests: Vec::with_capacity(capacity),
            paths: Vec::with_capacity(capacity),
            languages: Vec::with_capacity(capacity),
            content_hashes: Vec::with_capacity(capacity),
            source_bytes: Vec::with_capacity(capacity),
            payload_digests: Vec::with_capacity(capacity),
            payload_bytes: Vec::with_capacity(capacity),
        },
    };
    for (index, row) in batch.rows.iter().enumerate() {
        let ordinal = i32::try_from(index).map_err(|_| StorageError::InvalidInput {
            field: "spill_row_ordinal",
        })?;
        match row {
            NativeGenerationSpillExtractedRow::Inline(row) => {
                arrays.inline.ordinals.push(ordinal);
                arrays.inline.sort_keys.push(row.sort_key.clone());
                arrays.inline.payloads.push(row.payload.clone());
                arrays
                    .inline
                    .payload_digests
                    .push(row.payload_digest.as_str().to_owned());
            }
            NativeGenerationSpillExtractedRow::Cached(row) => {
                arrays.cached.ordinals.push(ordinal);
                arrays.cached.sort_keys.push(row.sort_key.clone());
                arrays
                    .cached
                    .contracts
                    .push(row.key.extractor_contract_digest().as_str().to_owned());
                arrays
                    .cached
                    .path_digests
                    .push(parse_cache_path_digest(row.key.path()));
                arrays.cached.paths.push(row.key.path().as_str().to_owned());
                arrays
                    .cached
                    .languages
                    .push(row.key.language().as_str().to_owned());
                arrays
                    .cached
                    .content_hashes
                    .push(row.key.content_hash().as_str().to_owned());
                arrays
                    .cached
                    .source_bytes
                    .push(as_i64(row.key.source_bytes(), "spill_cache_source_bytes")?);
                arrays
                    .cached
                    .payload_digests
                    .push(row.payload_digest.as_str().to_owned());
                arrays
                    .cached
                    .payload_bytes
                    .push(as_i64(row.payload_bytes, "spill_cache_payload_bytes")?);
            }
        }
    }
    Ok(arrays)
}

fn admit_spill_quota(
    report: NativeGenerationSpillReport,
    logical_bytes: u64,
    rows: u64,
) -> Result<(), StorageError> {
    report
        .logical_bytes
        .checked_add(logical_bytes)
        .filter(|value| *value <= report.maximum_bytes)
        .ok_or(StorageError::GenerationSpillLimitReached { resource: "bytes" })?;
    report
        .raw_rows
        .checked_add(rows)
        .filter(|value| *value <= report.maximum_rows)
        .ok_or(StorageError::GenerationSpillLimitReached { resource: "rows" })?;
    Ok(())
}

fn as_i64(value: u64, field: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::InvalidInput { field })
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

const fn corrupt(field: &'static str) -> StorageError {
    StorageError::CorruptStoredValue { field }
}

const fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn spill_policy_is_explicitly_bounded() {
        assert_eq!(
            NativeGenerationSpillPolicy::new(0, 1),
            Err(StorageError::InvalidInput {
                field: "maximum_spill_bytes"
            })
        );
        assert_eq!(
            NativeGenerationSpillPolicy::new(1, 0),
            Err(StorageError::InvalidInput {
                field: "maximum_spill_rows"
            })
        );
        assert!(NativeGenerationSpillPolicy::new(1, 1).is_ok());
    }

    /// A fact relation missing from either planner-column list would silently
    /// skip its `ANALYZE`, and its partitions, or later relations, would again
    /// be validated against statistics that do not describe this generation.
    #[test]
    fn every_fact_relation_refreshes_raw_and_canonical_planner_statistics() {
        for relation in NativeGenerationSpillRelation::FACTS {
            let raw = relation.raw_planner_statistics();
            assert_eq!(
                raw.map(|(table, _)| table),
                Some(format!("native_generation_spill_{}", relation.as_str())).as_deref(),
            );
            assert!(
                relation.canonical_planner_statistics().is_some(),
                "{} has no canonical planner statistics",
                relation.as_str()
            );
        }
        let extracted = NativeGenerationSpillRelation::ExtractedFiles;
        assert_eq!(extracted.raw_planner_statistics(), None);
        assert_eq!(extracted.canonical_planner_statistics(), None);
    }

    #[test]
    fn extracted_batch_digest_fences_sequence() {
        let row = NativeGenerationSpillRow::new(vec![1], vec![2])
            .unwrap_or_else(|error| panic!("fixture spill row must be valid: {error}"));
        let first = NativeGenerationSpillExtractedBatch::new(7, vec![row.clone().into()])
            .unwrap_or_else(|error| panic!("fixture spill batch must be valid: {error}"));
        let next = NativeGenerationSpillExtractedBatch::new(8, vec![row.into()])
            .unwrap_or_else(|error| panic!("fixture spill batch must be valid: {error}"));

        assert_ne!(first.digest, next.digest);
    }

    #[test]
    fn extracted_file_batches_are_bounded_contiguous_windows() {
        let first = NativeGenerationSpillRow::new(vec![1], vec![2])
            .unwrap_or_else(|error| panic!("fixture spill row must be valid: {error}"));
        let second = NativeGenerationSpillRow::new(vec![3], vec![4])
            .unwrap_or_else(|error| panic!("fixture spill row must be valid: {error}"));
        let batch = NativeGenerationSpillExtractedBatch::new(
            7,
            vec![first.clone().into(), second.clone().into()],
        )
        .unwrap_or_else(|error| panic!("multi-row extraction batch must be valid: {error}"));
        assert_eq!(batch.rows.len(), 2);
        assert_matches!(
            NativeGenerationSpillExtractedBatch::new(
                i64::MAX.unsigned_abs(),
                vec![first.into(), second.into()]
            )
            .err(),
            Some(StorageError::InvalidInput {
                field: "spill_batch_sequence"
            })
        );
    }
}
