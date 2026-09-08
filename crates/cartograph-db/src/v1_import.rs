use std::{
    borrow::Cow,
    collections::BTreeMap,
    future::Future,
    mem::size_of,
    path::{Path, PathBuf},
    time::Duration,
};

use cartograph_config::DatabaseSchema;
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, FileParseStatus,
    GenerationDigestVersion, GenerationId, GenerationState, NormalizedPath, ProjectId,
    ProjectOperation, SourceLanguage, SourceManifestDigestBuilder, SymbolId, SymbolKind,
    Visibility, project_root_identity, symbol_signature_is_search_safe,
};
use futures_util::TryStreamExt;
use serde::Serialize;
use sha2::{Digest as ShaDigest, Sha256};
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use thiserror::Error;
use tokio::{io::AsyncReadExt as _, time::Instant};

use crate::{
    CanonicalGenerationFacts, CartographDatabase, EdgeInput, FileInput, GenerationContents,
    GenerationFacts, GenerationRecoveryRequest, GenerationValidationLimits, LeaseOwner,
    LeaseRequest, LeaseTarget, PrepareGenerationMutation, RecoverableGeneration, ReferenceInput,
    SearchDocumentInput, SymbolInput, TerminalGenerationMutation, apply_page_rank,
    apply_sampled_betweenness,
};

mod legacy_json;
mod mapping;

use legacy_json::{
    has_valid_extraction_errors, parse_legacy_edge_metadata, parse_legacy_string_array,
    parse_legacy_u32_array,
};
use mapping::{map_source_facts, require_source_count, validate_v1_body_hash};

const V1_SCHEMA_VERSION: i64 = 75;
const MAXIMUM_IMPORT_ROWS: u64 = 100_000_000;
const MAXIMUM_IMPORT_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const MAXIMUM_IMPORT_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAXIMUM_LEGACY_ID_BYTES: usize = 4_096;
const MAXIMUM_LEGACY_METADATA_BYTES: usize = 64 * 1024;
const MAXIMUM_LEGACY_ERRORS_BYTES: usize = 1024 * 1024;
const MAXIMUM_LEGACY_SIGNATURE_BYTES: usize = 64 * 1024;
const MAXIMUM_LEGACY_DOCSTRING_BYTES: usize = 1024 * 1024;
const MAXIMUM_LEGACY_NAME_BYTES: usize = 4_096;
const MAXIMUM_LEGACY_QUALIFIED_NAME_BYTES: usize = 2_048;
const MAXIMUM_LEGACY_PATH_BYTES: usize = 4_096;
const MAXIMUM_LEGACY_LANGUAGE_BYTES: usize = 64;
const MAXIMUM_LEGACY_KIND_BYTES: usize = 64;
const MAXIMUM_LEGACY_VISIBILITY_BYTES: usize = 64;
const MAXIMUM_LEGACY_CONTENT_HASH_BYTES: usize = 128;
const MAXIMUM_LEGACY_BODY_HASH_BYTES: usize = 128;
const MAXIMUM_LEGACY_ARRAY_ITEMS: usize = 4_096;
const MAXIMUM_LEGACY_ERROR_ITEMS: usize = 4_096;
const MAXIMUM_LEGACY_JSON_DEPTH: usize = 32;
const SOURCE_ROW_ALLOCATION_ALLOWANCE: u64 = 512;
const DERIVED_ROW_ALLOCATION_ALLOWANCE: u64 = 1_024;
const FACT_MAPPING_BTREE_ENTRY_ALLOWANCE: usize = 128;
const FACT_MAPPING_FILE_PATH_COPIES: usize = 5;
const FACT_MAPPING_FILE_LANGUAGE_COPIES: usize = 2;
const FACT_MAPPING_LEGACY_ID_COPIES: usize = 3;
const FACT_MAPPING_QUALIFIED_NAME_COPIES: usize = 3;
const FACT_MAPPING_SIGNATURE_COPIES: usize = 2;
const REFERENCE_DYNAMIC_ROW_ALLOWANCE: usize = 256;
const PRIMARY_REFERENCE_ROW_COUNT: u64 = 1;
const SOURCE_JSON_EXPANSION_FACTOR: u64 = 16;
const SOURCE_LINE_OFFSET_BYTES: u64 = size_of::<u32>() as u64;
const SOURCE_PATH_TRANSIENT_BYTES: u64 = 4 * MAXIMUM_LEGACY_PATH_BYTES as u64;
// `sqlx` retains the current wire row while its text columns are decoded into owned strings.
// A maximally sized node is the largest source row admitted by the schema preflight.
const SOURCE_ROW_DECODE_TRANSIENT_BYTES: u64 = (MAXIMUM_LEGACY_ID_BYTES
    + MAXIMUM_LEGACY_NAME_BYTES
    + MAXIMUM_LEGACY_KIND_BYTES
    + MAXIMUM_LEGACY_QUALIFIED_NAME_BYTES
    + MAXIMUM_LEGACY_PATH_BYTES
    + MAXIMUM_LEGACY_LANGUAGE_BYTES
    + MAXIMUM_LEGACY_DOCSTRING_BYTES
    + MAXIMUM_LEGACY_SIGNATURE_BYTES
    + MAXIMUM_LEGACY_BODY_HASH_BYTES
    + MAXIMUM_LEGACY_VISIBILITY_BYTES) as u64;
const V1_BODY_HASH_TRANSIENT_BYTES: u64 = 24 * 1024;
const V1_MAX_BODY_UTF16_UNITS: usize = 4_000;
const IMPORT_LOCK_NAMESPACE: &str = "cartograph-v1.1.33-import";
const BM25_REBUILD_LOCK_NAMESPACE: &str = "cartograph-v2-bm25-rebuild";
const LEGACY_PROVENANCE: &str = "v1.1.33-postgres";
const UUID_VERSION_BYTE: usize = 6;
const UUID_VARIANT_BYTE: usize = 8;
const UUID_BYTE_LENGTH: usize = 16;
const UUID_VERSION_CLEAR_MASK: u8 = 0x0f;
const UUID_VERSION_EIGHT: u8 = 0x80;
const UUID_VARIANT_CLEAR_MASK: u8 = 0x3f;
const UUID_VARIANT_RFC_4122: u8 = 0x80;
const EXTRACTED_CONFIDENCE: f32 = 1.0;
const INFERRED_CONFIDENCE: f32 = 0.75;
const AMBIGUOUS_CONFIDENCE: f32 = 0.5;
const UNRESOLVED_CONFIDENCE: f32 = 0.0;
const IMPORT_WORKER_COUNT: i16 = 1;
const EXPECTED_MUTATED_ROWS: u64 = 1;
const V1_SIGNATURE_SEPARATOR: [u8; 1] = [0];
const V1_BODY_HASH_HEX_LENGTH: usize = 32;

fn line_starts(source: &str) -> Result<Vec<u32>, V1PostgresImportError> {
    let line_count = source
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .checked_add(1)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let mut starts = Vec::new();
    starts
        .try_reserve_exact(line_count)
        .map_err(|_| V1PostgresImportError::SourceLimit)?;
    starts.push(0);
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(
                u32::try_from(index.saturating_add(1))
                    .map_err(|_| V1PostgresImportError::SourceLimit)?,
            );
        }
    }
    Ok(starts)
}
const HEX_NIBBLE_BITS: u8 = 4;
const HEX_NIBBLE_MASK: u8 = 0x0f;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const HEX_CHARACTERS_PER_BYTE: usize = 2;
const SOURCE_PREFLIGHT_SQL_TEMPLATE: &str = r#"SELECT
    (SELECT COALESCE(max(version), 0)::bigint FROM {source}."schema_versions") AS schema_version,
    (SELECT count(*)::bigint FROM {source}."files") AS file_count,
    (SELECT count(*)::bigint FROM {source}."nodes") AS symbol_count,
    (SELECT count(*)::bigint FROM {source}."edges" WHERE kind <> 'similar_to') AS edge_count,
    (SELECT count(*)::bigint FROM {source}."unresolved_refs") AS reference_count,
    (SELECT COALESCE(sum(size), 0)::bigint FROM {source}."files") AS source_bytes,
    (
        (SELECT COALESCE(sum(
            octet_length(path)::bigint + octet_length(content_hash) + octet_length(language)
            + octet_length(COALESCE(errors, ''))
        ), 0)::bigint FROM {source}."files")
        + (SELECT COALESCE(sum(
            octet_length(id)::bigint + octet_length(name) + octet_length(kind)
            + octet_length(qualified_name)
            + octet_length(file_path) + octet_length(language)
            + octet_length(COALESCE(docstring, ''))
            + octet_length(COALESCE(signature, '')) + octet_length(body_hash)
            + octet_length(COALESCE(visibility, ''))
        ), 0)::bigint FROM {source}."nodes")
        + (SELECT COALESCE(sum(
            octet_length(source)::bigint + octet_length(target) + octet_length(kind)
            + octet_length(COALESCE(confidence, ''))
            + octet_length(COALESCE(metadata, ''))
        ), 0)::bigint FROM {source}."edges" WHERE kind <> 'similar_to')
        + (SELECT COALESCE(sum(
            octet_length(from_node_id)::bigint + octet_length(reference_name)
            + octet_length(reference_kind) + octet_length(file_path)
            + octet_length(language) + octet_length(COALESCE(candidates, ''))
            + octet_length(COALESCE(extra_lines, ''))
        ), 0)::bigint FROM {source}."unresolved_refs")
    )::bigint AS source_metadata_bytes,
    (
        (SELECT COALESCE(sum(octet_length(COALESCE(errors, ''))), 0)::bigint
            FROM {source}."files")
        + (SELECT COALESCE(sum(octet_length(COALESCE(metadata, ''))), 0)::bigint
            FROM {source}."edges" WHERE kind <> 'similar_to')
        + (SELECT COALESCE(sum(
            octet_length(COALESCE(candidates, ''))
            + octet_length(COALESCE(extra_lines, ''))
        ), 0)::bigint FROM {source}."unresolved_refs")
    )::bigint AS source_json_bytes,
    (
        (SELECT count(*) FROM {source}."files" WHERE
            octet_length(path) NOT BETWEEN 1 AND {max_path}
            OR octet_length(content_hash) NOT BETWEEN 1 AND {max_content_hash}
            OR octet_length(language) NOT BETWEEN 1 AND {max_language}
            OR octet_length(COALESCE(errors, '')) > {max_errors}
            OR size < 0 OR size > {max_file_bytes})
        + (SELECT count(*) FROM {source}."nodes" WHERE
            octet_length(id) NOT BETWEEN 1 AND {max_id}
            OR octet_length(name) NOT BETWEEN 1 AND {max_name}
            OR octet_length(kind) NOT BETWEEN 1 AND {max_kind}
            OR octet_length(qualified_name) NOT BETWEEN 1 AND {max_qualified_name}
            OR octet_length(file_path) NOT BETWEEN 1 AND {max_path}
            OR octet_length(language) NOT BETWEEN 1 AND {max_language}
            OR octet_length(COALESCE(docstring, '')) > {max_docstring}
            OR octet_length(COALESCE(signature, '')) > {max_signature}
            OR octet_length(body_hash) > {max_body_hash}
            OR octet_length(COALESCE(visibility, '')) > {max_visibility}
            OR COALESCE(is_exported NOT IN (0, 1), false)
            OR COALESCE(is_default_export NOT IN (0, 1), false)
            OR COALESCE(is_async NOT IN (0, 1), false)
            OR COALESCE(is_static NOT IN (0, 1), false))
        + (SELECT count(*) FROM {source}."edges" WHERE kind <> 'similar_to' AND (
            octet_length(source) NOT BETWEEN 1 AND {max_id}
            OR octet_length(target) NOT BETWEEN 1 AND {max_id}
            OR octet_length(kind) NOT BETWEEN 1 AND {max_kind}
            OR octet_length(COALESCE(confidence, '')) > {max_kind}
            OR octet_length(COALESCE(metadata, '')) > {max_metadata}))
        + (SELECT count(*) FROM {source}."unresolved_refs" WHERE
            octet_length(from_node_id) NOT BETWEEN 1 AND {max_id}
            OR octet_length(reference_name) NOT BETWEEN 1 AND {max_name}
            OR octet_length(reference_kind) NOT BETWEEN 1 AND {max_kind}
            OR octet_length(file_path) NOT BETWEEN 1 AND {max_path}
            OR octet_length(language) NOT BETWEEN 1 AND {max_language}
            OR octet_length(COALESCE(candidates, '')) > {max_metadata}
            OR octet_length(COALESCE(extra_lines, '')) > {max_metadata}
            OR site_count < 1)
    )::bigint AS oversized_field_count,
    (SELECT count(*)::bigint FROM {source}."files"
        WHERE COALESCE(needs_reextract, -1) <> 0) AS needs_reextract_count,
    (SELECT count(*)::bigint FROM {source}."nodes" AS nodes
        LEFT JOIN {source}."files" AS files ON files.path = nodes.file_path
        WHERE files.path IS NULL) AS orphan_node_count,
    (SELECT count(*)::bigint FROM {source}."edges" AS edges
        LEFT JOIN {source}."nodes" AS sources ON sources.id = edges.source
        LEFT JOIN {source}."nodes" AS targets ON targets.id = edges.target
        WHERE edges.kind <> 'similar_to'
          AND (sources.id IS NULL OR targets.id IS NULL)) AS orphan_edge_count,
    (SELECT count(*)::bigint FROM {source}."unresolved_refs" AS refs
        LEFT JOIN {source}."nodes" AS nodes ON nodes.id = refs.from_node_id
        LEFT JOIN {source}."files" AS files ON files.path = refs.file_path
        WHERE nodes.id IS NULL OR files.path IS NULL) AS orphan_reference_count,
    (SELECT count(*)::bigint FROM {source}."files" AS files
        WHERE COALESCE(files.node_count, -1) <> (
            SELECT count(*) FROM {source}."nodes" AS nodes
            WHERE nodes.file_path = files.path
        )) AS node_count_mismatch,
    (
        (SELECT count(*) FROM {source}."files"
            WHERE NOT (language = ANY(CAST($1 AS text[]))))
        + (SELECT count(*) FROM {source}."nodes"
            WHERE NOT (language = ANY(CAST($1 AS text[]))))
        + (SELECT count(*) FROM {source}."unresolved_refs"
            WHERE NOT (language = ANY(CAST($1 AS text[]))))
    )::bigint AS unsupported_language_count,
    (SELECT count(*) BETWEEN 1 AND {version}
            AND COALESCE(max(version), 0) = {version}
            AND COALESCE(bool_and(version BETWEEN 1 AND {version}), false)
        FROM {source}."schema_versions") AS valid_schema_history"#;

/// Explicit input and memory admission bounds for one v1 PostgreSQL import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V1PostgresImportLimits {
    maximum_rows: u64,
    maximum_source_bytes: u64,
    validation: GenerationValidationLimits,
}

impl V1PostgresImportLimits {
    /// Build hard source-row, source-byte, and canonical validation ceilings.
    /// # Errors
    ///
    /// Returns an error if source-row or source-byte limits are zero or exceed
    /// the importer hard ceilings.
    pub const fn new(
        maximum_rows: u64,
        maximum_source_bytes: u64,
        validation: GenerationValidationLimits,
    ) -> Result<Self, V1PostgresImportError> {
        if maximum_rows == 0
            || maximum_rows > MAXIMUM_IMPORT_ROWS
            || maximum_source_bytes == 0
            || maximum_source_bytes > MAXIMUM_IMPORT_SOURCE_BYTES
        {
            return Err(V1PostgresImportError::InvalidInput {
                field: "import_limits",
            });
        }
        Ok(Self {
            maximum_rows,
            maximum_source_bytes,
            validation,
        })
    }
}

/// Immutable v1 source identity and checkout binding.
#[derive(Clone)]
pub struct V1PostgresSource {
    source_schema: DatabaseSchema,
    project_root: PathBuf,
    revision: V1PostgresSourceRevision,
}

/// Repository identity and caller-owned exact v1 source-manifest assertion.
#[derive(Clone)]
pub struct V1PostgresSourceRevision {
    repository_fingerprint: ContentDigest,
    expected_source_manifest: ContentDigest,
}

impl V1PostgresSourceRevision {
    /// Pair a stable repository identity with an independently known raw
    /// source-manifest digest.
    #[must_use]
    pub const fn new(
        repository_fingerprint: ContentDigest,
        source_revision: ContentDigest,
    ) -> Self {
        Self {
            repository_fingerprint,
            expected_source_manifest: source_revision,
        }
    }
}

impl V1PostgresSource {
    /// Bind one explicit v1 schema to the checkout and revision it describes.
    #[must_use]
    pub fn new(
        source_schema: DatabaseSchema,
        project_root: impl Into<PathBuf>,
        revision: V1PostgresSourceRevision,
    ) -> Self {
        Self {
            source_schema,
            project_root: project_root.into(),
            revision,
        }
    }
}

/// Lease owner and bounded PostgreSQL deadlines for one import invocation.
#[derive(Clone)]
pub struct V1PostgresImportExecution {
    owner: LeaseOwner,
    lease_duration: Duration,
    statement_timeout: Duration,
}

impl V1PostgresImportExecution {
    /// Bind an import to one lease owner and bounded database deadlines.
    #[must_use]
    pub const fn new(
        owner: LeaseOwner,
        lease_duration: Duration,
        statement_timeout: Duration,
    ) -> Self {
        Self {
            owner,
            lease_duration,
            statement_timeout,
        }
    }
}

/// PostgreSQL-only v1.1.33 cutover request.
#[derive(Clone)]
pub struct V1PostgresImportRequest {
    source: V1PostgresSource,
    execution: V1PostgresImportExecution,
    limits: V1PostgresImportLimits,
}

impl V1PostgresImportRequest {
    /// Combine source identity, execution policy, and admission limits.
    #[must_use]
    pub fn new(
        source: V1PostgresSource,
        execution: V1PostgresImportExecution,
        limits: V1PostgresImportLimits,
    ) -> Self {
        Self {
            source,
            execution,
            limits,
        }
    }
}

/// Structural rows admitted from the immutable v1 source snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct V1PostgresImportCounts {
    /// Source files.
    pub files: u64,
    /// Structural symbols (`nodes` in v1).
    pub symbols: u64,
    /// Non-derived graph edges; v1 `similar_to` cache rows are regenerated.
    pub edges: u64,
    /// Exact relation sites represented by the canonical graph-edge rows.
    pub edge_sites: u64,
    /// Canonical resolved and unresolved reference rows.
    pub references: u64,
    /// Exact resolved and unresolved source sites represented by reference rows.
    pub reference_sites: u64,
    /// Search documents derived from every imported file and symbol.
    pub documents: u64,
}

impl V1PostgresImportCounts {
    fn total(self) -> Option<u64> {
        self.files
            .checked_add(self.symbols)?
            .checked_add(self.edges)?
            .checked_add(self.references)?
            .checked_add(self.documents)
    }
}

/// Observable importer milestones. Durable milestones are append-only in PostgreSQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum V1PostgresImportCheckpoint {
    /// Source rows, relations, hashes, and canonical v2 facts validated without mutation.
    Validated,
    /// A deterministic staging generation and run record committed.
    Staged,
    /// Canonical facts committed and the generation became ready.
    Ready,
    /// The schema-wide derived BM25 index was transactionally rebuilt.
    Bm25Rebuilt,
    /// The imported generation was atomically published.
    Complete,
}

impl V1PostgresImportCheckpoint {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Validated => "validated",
            Self::Staged => "staged",
            Self::Ready => "ready",
            Self::Bm25Rebuilt => "bm25_rebuilt",
            Self::Complete => "complete",
        }
    }

    fn parse(raw: &str) -> Result<Self, V1PostgresImportError> {
        match raw {
            "staged" => Ok(Self::Staged),
            "ready" => Ok(Self::Ready),
            "bm25_rebuilt" => Ok(Self::Bm25Rebuilt),
            "complete" => Ok(Self::Complete),
            _ => Err(V1PostgresImportError::CorruptCheckpoint),
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Validated => 0,
            Self::Staged => 1,
            Self::Ready => 2,
            Self::Bm25Rebuilt => 3,
            Self::Complete => 4,
        }
    }
}

/// Read-only proof that a v1.1.33 PostgreSQL source can become one canonical v2 generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct V1PostgresDryRunReport {
    /// Raw path/content manifest of the exact verified v1 file set.
    pub source_revision: ContentDigest,
    /// BLAKE3 fingerprint covering the explicit schema and every imported source field.
    pub source_fingerprint: ContentDigest,
    /// Canonical v2 logical generation digest.
    pub content_digest: ContentDigest,
    /// Exact canonical row counts.
    pub counts: V1PostgresImportCounts,
    /// Checkout bytes whose v1 SHA-256 hashes were independently verified.
    pub source_bytes: u64,
}

/// Completed or already-completed cutover result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct V1PostgresImportReport {
    /// Destination project identity.
    pub project_id: ProjectId,
    /// Deterministic import-generation identity.
    pub generation_id: GenerationId,
    /// Immutable source fingerprint recorded by migration 7.
    pub source_fingerprint: ContentDigest,
    /// Published canonical v2 digest.
    pub content_digest: ContentDigest,
    /// Exact source/destination counts.
    pub counts: V1PostgresImportCounts,
    /// Whether this invocation resumed a durable run.
    pub resumed: bool,
    /// Final durable checkpoint.
    pub checkpoint: V1PostgresImportCheckpoint,
}

/// Credential- and source-safe v1 importer failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum V1PostgresImportError {
    /// A bounded caller input is invalid; its value is deliberately omitted.
    #[error("invalid {field} in Cartograph v1 PostgreSQL import request")]
    InvalidInput {
        /// Stable field name.
        field: &'static str,
    },
    /// Source and destination schemas must be distinct.
    #[error("v1 PostgreSQL source schema must differ from the v2 destination schema")]
    SameSchema,
    /// The source is not the frozen v1.1.33 PostgreSQL schema contract.
    #[error("PostgreSQL source is not a complete Cartograph v1.1.33 schema")]
    UnsupportedSourceSchema,
    /// Preflight row or source-byte counts exceed caller policy.
    #[error("v1 PostgreSQL import exceeds its configured source limit")]
    SourceLimit,
    /// A source path, row, span, enum, or stored count is invalid.
    #[error("v1 PostgreSQL source violates the {field} contract")]
    InvalidSourceData {
        /// Stable source field/invariant name.
        field: &'static str,
    },
    /// Checkout bytes do not match v1's stored SHA-256 content hash.
    #[error("v1 PostgreSQL source content hash does not match the checkout")]
    ContentHashMismatch,
    /// The exact imported path/content set differs from the requested live source manifest.
    #[error("v1 PostgreSQL source does not match the requested source manifest")]
    SourceManifestMismatch,
    /// Source facts did not pass canonical v2 relation and retained-memory validation.
    #[error("v1 PostgreSQL source cannot be represented as a canonical v2 generation")]
    CanonicalValidation,
    /// A prior run for this project/schema recorded different immutable source evidence.
    #[error("v1 PostgreSQL import source changed after its first durable checkpoint")]
    SourceFingerprintChanged,
    /// A durable run/generation checkpoint is internally inconsistent.
    #[error("v1 PostgreSQL import checkpoint state is corrupt")]
    CorruptCheckpoint,
    /// Cooperative interruption was requested at a durable milestone.
    #[error("v1 PostgreSQL import was interrupted and can be resumed")]
    Interrupted,
    /// Source snapshot loading or canonical analysis exceeded the configured deadline.
    #[error("Cartograph v1 PostgreSQL source analysis exceeded its configured deadline")]
    AnalysisDeadline,
    /// The exact migration lease expired or was replaced before a fenced mutation committed.
    #[error("Cartograph v1 PostgreSQL import lost its migration lease fence")]
    LeaseFenceLost,
    /// Another writer published after this import reserved its generation sequence.
    #[error("another Cartograph writer published during v1 import; retry after it is idle")]
    ConcurrentPublication,
    /// PostgreSQL failed without rendering query, row, schema, or credentials.
    #[error("Cartograph PostgreSQL v1 import failed during {operation}")]
    DatabaseOperation {
        /// Stable operation label.
        operation: &'static str,
    },
}

struct V1Analysis {
    report: V1PostgresDryRunReport,
    facts: Option<CanonicalGenerationFacts>,
}

struct SourceSnapshot {
    files: BTreeMap<String, SourceFile>,
    nodes: Vec<SourceNode>,
    edges: Vec<SourceEdge>,
    references: Vec<SourceReference>,
    source_revision: ContentDigest,
    fingerprint: ContentDigest,
    source_bytes: u64,
}

struct SourceFile {
    language: String,
    source: String,
    line_starts: Vec<u32>,
    content_hash: ContentDigest,
    has_extraction_errors: bool,
    is_test: bool,
}

struct SourceNode {
    legacy_id: String,
    name: String,
    kind: String,
    qualified_name: String,
    file_path: String,
    language: String,
    start_line: u32,
    end_line: u32,
    start_column: u32,
    end_column: u32,
    docstring: Option<String>,
    signature: Option<String>,
    body_hash: String,
    visibility: Option<String>,
    export: cartograph_domain::SymbolExportFlags,
    execution: cartograph_domain::SymbolExecutionFlags,
}

struct SourceEdge {
    source: String,
    target: String,
    kind: String,
    confidence: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
    metadata: LegacyEdgeMetadata,
}

struct SourceReference {
    from_node_id: String,
    reference_name: String,
    reference_kind: String,
    line: u32,
    column: u32,
    file_path: String,
    language: String,
    candidates: Vec<String>,
    site_count: u32,
    extra_lines: Vec<u32>,
}

struct DecodedSourceReference {
    reference: SourceReference,
    raw_candidates: Option<String>,
    raw_extra_lines: Option<String>,
}

#[derive(Clone, Copy)]
struct ReferenceHashEvidence<'a> {
    reference: &'a SourceReference,
    raw_candidates: Option<&'a str>,
    raw_extra_lines: Option<&'a str>,
}

struct LegacyEdgeMetadata {
    provenance: Option<String>,
    numeric_confidence: Option<f32>,
    site_count: u32,
    extra_lines: Vec<u32>,
    def_use_name: Option<String>,
    def_use_lines: Vec<u32>,
}

impl Default for LegacyEdgeMetadata {
    fn default() -> Self {
        Self {
            provenance: None,
            numeric_confidence: None,
            site_count: 1,
            extra_lines: Vec::new(),
            def_use_name: None,
            def_use_lines: Vec::new(),
        }
    }
}

struct SourcePreflight {
    counts: V1PostgresImportCounts,
    source_bytes: u64,
    actual_checkout_byte_budget: u64,
}

struct SourcePreflightEvidence {
    counts: V1PostgresImportCounts,
    source_bytes: u64,
    source_metadata_bytes: u64,
    source_json_bytes: u64,
}

#[derive(Clone, Copy)]
struct PreflightMemoryInput {
    counts: V1PostgresImportCounts,
    source_bytes: u64,
    source_metadata_bytes: u64,
    source_json_bytes: u64,
    maximum_source_bytes: u64,
    maximum_working_bytes: u64,
}

struct ImportRun {
    generation_id: GenerationId,
    checkpoint: V1PostgresImportCheckpoint,
    checkpoint_history: Vec<V1PostgresImportCheckpoint>,
    resumed: bool,
}

struct InitializedImport {
    analysis: V1Analysis,
    project_id: ProjectId,
    run: ImportRun,
}

struct ActiveImport {
    initialized: InitializedImport,
    generation_state: GenerationState,
}

enum ImportDisposition {
    Complete(V1PostgresImportReport),
    Active(Box<ActiveImport>),
}

struct PublicationCandidate {
    ready: crate::ReadyGeneration,
    checkpoint: V1PostgresImportCheckpoint,
}

struct CompletedImportReconciliation {
    initialized: InitializedImport,
    state: Option<GenerationState>,
    statement_timeout: Duration,
}

struct LeaseReleaseFailure<'a> {
    lease: &'a crate::ProjectLease,
    statement_timeout: Duration,
    error: V1PostgresImportError,
}

struct ImportReportInput<'a> {
    project_id: ProjectId,
    generation_id: GenerationId,
    report: &'a V1PostgresDryRunReport,
    resumed: bool,
}

struct GenerationLocator<'a> {
    project_id: &'a ProjectId,
    generation_id: &'a GenerationId,
}

struct DestinationVerification<'a> {
    locator: GenerationLocator<'a>,
    analysis: &'a V1Analysis,
}

struct Bm25Rebuild<'a> {
    fence: &'a crate::LeaseFence,
    content_digest: &'a ContentDigest,
    statement_timeout: Duration,
}

struct ActiveImportInvocation<'a> {
    request: &'a V1PostgresImportRequest,
    active: ActiveImport,
}

struct LeasedImportContext<'a> {
    database: &'a CartographDatabase,
    execution: &'a V1PostgresImportExecution,
    active: &'a mut ActiveImport,
    lease: &'a mut crate::ProjectLease,
    fence: &'a crate::LeaseFence,
}

struct HeartbeatContext<'a> {
    database: &'a CartographDatabase,
    execution: &'a V1PostgresImportExecution,
    lease: &'a mut crate::ProjectLease,
}

impl InitializedImport {
    const fn locator(&self) -> GenerationLocator<'_> {
        GenerationLocator {
            project_id: &self.project_id,
            generation_id: &self.run.generation_id,
        }
    }
}

impl CartographDatabase {
    /// Validate a frozen v1.1.33 PostgreSQL schema and checkout without mutation.
    /// # Errors
    ///
    /// Returns an error if source schema/version, frozen checkout bytes,
    /// identities, row/byte bounds, or canonical validation do not match.
    pub async fn dry_run_v1_postgres_import(
        &self,
        request: &V1PostgresImportRequest,
    ) -> Result<V1PostgresDryRunReport, V1PostgresImportError> {
        analyze_source(self, request)
            .await
            .map(|analysis| analysis.report)
    }

    /// Import and publish, resuming any exact matching durable checkpoint.
    /// # Errors
    ///
    /// Returns an error if preflight or confirmation fails, a lease/checkpoint
    /// is incompatible, publication races, or any durable import stage fails.
    pub async fn import_v1_postgres(
        &self,
        request: V1PostgresImportRequest,
    ) -> Result<V1PostgresImportReport, V1PostgresImportError> {
        self.import_v1_postgres_with_observer(request, |_| false)
            .await
    }

    /// Import with cooperative interruption at explicit durable milestones.
    pub fn import_v1_postgres_with_observer<'database, Observe>(
        &'database self,
        request: V1PostgresImportRequest,
        mut interrupt: Observe,
    ) -> impl Future<Output = Result<V1PostgresImportReport, V1PostgresImportError>> + 'database
    where
        Observe: FnMut(V1PostgresImportCheckpoint) -> bool + 'database,
    {
        Box::pin(async move {
            let initialized = initialize_import(self, &request, &mut interrupt).await?;
            match reconcile_initialized_import(
                self,
                initialized,
                request.execution.statement_timeout,
            )
            .await?
            {
                ImportDisposition::Complete(report) => Ok(report),
                ImportDisposition::Active(active) => {
                    Box::pin(execute_active_import(
                        self,
                        ActiveImportInvocation {
                            request: &request,
                            active: *active,
                        },
                        &mut interrupt,
                    ))
                    .await
                }
            }
        })
    }
}

async fn initialize_import<Observe>(
    database: &CartographDatabase,
    request: &V1PostgresImportRequest,
    interrupt: &mut Observe,
) -> Result<InitializedImport, V1PostgresImportError>
where
    Observe: FnMut(V1PostgresImportCheckpoint) -> bool,
{
    let analysis = analyze_source(database, request).await?;
    interrupt_at(interrupt, V1PostgresImportCheckpoint::Validated)?;
    database
        .migrate_bounded(request.execution.statement_timeout)
        .await
        .map_err(|_| database_error("migrate-destination"))?;
    let root_identity = project_root_identity(&request.source.revision.repository_fingerprint);
    let project_id = register_import_project(database, request, &root_identity).await?;
    let run = initialize_import_run(
        database,
        ImportRunInitialization {
            request,
            report: &analysis.report,
            project_id: &project_id,
            destination: crate::database::quoted_schema(&database.schema),
        },
    )
    .await?;
    interrupt_at(interrupt, V1PostgresImportCheckpoint::Staged)?;
    Ok(InitializedImport {
        analysis,
        project_id,
        run,
    })
}

async fn reconcile_initialized_import(
    database: &CartographDatabase,
    initialized: InitializedImport,
    statement_timeout: Duration,
) -> Result<ImportDisposition, V1PostgresImportError> {
    let state = load_generation_state(database, initialized.locator(), statement_timeout).await?;
    validate_checkpoint_state(&initialized.run, state)?;
    if initialized.run.checkpoint == V1PostgresImportCheckpoint::Complete {
        return reconcile_completed_import(
            database,
            CompletedImportReconciliation {
                initialized,
                state,
                statement_timeout,
            },
        )
        .await;
    }
    let state = state.ok_or(V1PostgresImportError::CorruptCheckpoint)?;
    if matches!(
        state,
        GenerationState::Current | GenerationState::Superseded
    ) {
        if initialized.run.checkpoint != V1PostgresImportCheckpoint::Bm25Rebuilt {
            return Err(V1PostgresImportError::CorruptCheckpoint);
        }
        verify_destination(
            database,
            DestinationVerification {
                locator: initialized.locator(),
                analysis: &initialized.analysis,
            },
            statement_timeout,
        )
        .await?;
        append_checkpoint(
            database,
            CheckpointAdvance {
                import_id: &initialized.run.generation_id,
                previous: initialized.run.checkpoint,
                next: V1PostgresImportCheckpoint::Complete,
            },
            statement_timeout,
        )
        .await?;
        return Ok(ImportDisposition::Complete(completed_report(initialized)));
    }
    if state == GenerationState::Failed {
        return Err(V1PostgresImportError::CorruptCheckpoint);
    }
    Ok(ImportDisposition::Active(Box::new(ActiveImport {
        initialized,
        generation_state: state,
    })))
}

async fn reconcile_completed_import(
    database: &CartographDatabase,
    reconciliation: CompletedImportReconciliation,
) -> Result<ImportDisposition, V1PostgresImportError> {
    match reconciliation.state {
        Some(GenerationState::Current | GenerationState::Superseded) => {
            verify_destination(
                database,
                DestinationVerification {
                    locator: reconciliation.initialized.locator(),
                    analysis: &reconciliation.initialized.analysis,
                },
                reconciliation.statement_timeout,
            )
            .await?;
        }
        None | Some(GenerationState::Retiring) => {}
        Some(GenerationState::Staging | GenerationState::Ready | GenerationState::Failed) => {
            return Err(V1PostgresImportError::CorruptCheckpoint);
        }
    }
    Ok(ImportDisposition::Complete(completed_report(
        reconciliation.initialized,
    )))
}

async fn execute_active_import<Observe>(
    database: &CartographDatabase,
    mut invocation: ActiveImportInvocation<'_>,
    interrupt: &mut Observe,
) -> Result<V1PostgresImportReport, V1PostgresImportError>
where
    Observe: FnMut(V1PostgresImportCheckpoint) -> bool,
{
    let target = LeaseTarget::new(
        invocation.active.initialized.project_id.clone(),
        ProjectOperation::Migration,
        Some(invocation.active.initialized.run.generation_id.clone()),
    );
    let mut lease = database
        .acquire_lease_bounded(
            LeaseRequest::new(
                target,
                invocation.request.execution.owner.clone(),
                invocation.request.execution.lease_duration,
            ),
            invocation.request.execution.statement_timeout,
        )
        .await
        .map_err(|_| database_error("acquire-migration-lease"))?;
    let fence = lease.fence();
    let candidate = prepare_import_for_publication(
        &mut LeasedImportContext {
            database,
            execution: &invocation.request.execution,
            active: &mut invocation.active,
            lease: &mut lease,
            fence: &fence,
        },
        interrupt,
    )
    .await;
    let candidate = match candidate {
        Ok(candidate) => candidate,
        Err(error) => {
            return release_and_error(
                database,
                LeaseReleaseFailure {
                    lease: &lease,
                    statement_timeout: invocation.request.execution.statement_timeout,
                    error,
                },
            )
            .await;
        }
    };
    if let Err(error) = database
        .publish_generation_bounded(
            candidate.ready,
            TerminalGenerationMutation::new(&fence, invocation.request.execution.statement_timeout),
        )
        .await
    {
        let (_, storage_error) = error.into_parts();
        if matches!(storage_error, crate::StorageError::StaleGeneration { .. }) {
            database
                .fail_generation_and_release_bounded(TerminalGenerationMutation::new(
                    &fence,
                    invocation.request.execution.statement_timeout,
                ))
                .await
                .map_err(|error| storage_mutation_error(&error, "recover-stale-import"))?;
            return Err(V1PostgresImportError::ConcurrentPublication);
        }
        return release_and_error(
            database,
            LeaseReleaseFailure {
                lease: &lease,
                statement_timeout: invocation.request.execution.statement_timeout,
                error: storage_mutation_error(&storage_error, "publish-generation"),
            },
        )
        .await;
    }
    append_checkpoint(
        database,
        CheckpointAdvance {
            import_id: &invocation.active.initialized.run.generation_id,
            previous: candidate.checkpoint,
            next: V1PostgresImportCheckpoint::Complete,
        },
        invocation.request.execution.statement_timeout,
    )
    .await?;
    // `publish_generation_bounded` atomically deletes the exact generation fence.
    let _ = interrupt(V1PostgresImportCheckpoint::Complete);
    Ok(import_report(ImportReportInput {
        project_id: invocation.active.initialized.project_id,
        generation_id: invocation.active.initialized.run.generation_id,
        report: &invocation.active.initialized.analysis.report,
        resumed: invocation.active.initialized.run.resumed,
    }))
}

async fn prepare_import_for_publication<Observe>(
    context: &mut LeasedImportContext<'_>,
    interrupt: &mut Observe,
) -> Result<PublicationCandidate, V1PostgresImportError>
where
    Observe: FnMut(V1PostgresImportCheckpoint) -> bool,
{
    validate_active_checkpoint(context.active)?;
    let ready = recover_import_generation(context).await?;
    run_with_heartbeat(
        &mut HeartbeatContext {
            database: context.database,
            execution: context.execution,
            lease: &mut *context.lease,
        },
        verify_destination(
            context.database,
            DestinationVerification {
                locator: context.active.initialized.locator(),
                analysis: &context.active.initialized.analysis,
            },
            context.execution.statement_timeout,
        ),
    )
    .await??;
    let mut checkpoint = context.active.initialized.run.checkpoint;
    if checkpoint.rank() < V1PostgresImportCheckpoint::Ready.rank() {
        append_checkpoint(
            context.database,
            CheckpointAdvance {
                import_id: &context.active.initialized.run.generation_id,
                previous: checkpoint,
                next: V1PostgresImportCheckpoint::Ready,
            },
            context.execution.statement_timeout,
        )
        .await?;
        checkpoint = V1PostgresImportCheckpoint::Ready;
    }
    interrupt_at(interrupt, V1PostgresImportCheckpoint::Ready)?;
    if checkpoint.rank() < V1PostgresImportCheckpoint::Bm25Rebuilt.rank() {
        heartbeat_import_lease(context).await?;
        rebuild_bm25(
            context.database,
            Bm25Rebuild {
                fence: context.fence,
                content_digest: &context.active.initialized.analysis.report.content_digest,
                statement_timeout: context.execution.statement_timeout,
            },
        )
        .await?;
        append_checkpoint(
            context.database,
            CheckpointAdvance {
                import_id: &context.active.initialized.run.generation_id,
                previous: checkpoint,
                next: V1PostgresImportCheckpoint::Bm25Rebuilt,
            },
            context.execution.statement_timeout,
        )
        .await?;
        checkpoint = V1PostgresImportCheckpoint::Bm25Rebuilt;
    }
    interrupt_at(interrupt, V1PostgresImportCheckpoint::Bm25Rebuilt)?;
    Ok(PublicationCandidate { ready, checkpoint })
}

async fn heartbeat_import_lease(
    context: &mut LeasedImportContext<'_>,
) -> Result<(), V1PostgresImportError> {
    let heartbeat_interval = context.execution.lease_duration / 3;
    let heartbeat_timeout = context.execution.statement_timeout.min(heartbeat_interval);
    context
        .database
        .heartbeat_lease_bounded(context.lease, heartbeat_timeout)
        .await
        .map_err(|error| lease_heartbeat_error(&error))
}

fn interrupt_at<Observe>(
    interrupt: &mut Observe,
    checkpoint: V1PostgresImportCheckpoint,
) -> Result<(), V1PostgresImportError>
where
    Observe: FnMut(V1PostgresImportCheckpoint) -> bool,
{
    if interrupt(checkpoint) {
        Err(V1PostgresImportError::Interrupted)
    } else {
        Ok(())
    }
}

fn completed_report(initialized: InitializedImport) -> V1PostgresImportReport {
    import_report(ImportReportInput {
        project_id: initialized.project_id,
        generation_id: initialized.run.generation_id,
        report: &initialized.analysis.report,
        resumed: true,
    })
}

fn validate_active_checkpoint(active: &ActiveImport) -> Result<(), V1PostgresImportError> {
    let checkpoint = active.initialized.run.checkpoint;
    let valid = match active.generation_state {
        GenerationState::Staging => checkpoint == V1PostgresImportCheckpoint::Staged,
        GenerationState::Ready => checkpoint != V1PostgresImportCheckpoint::Complete,
        GenerationState::Current
        | GenerationState::Superseded
        | GenerationState::Failed
        | GenerationState::Retiring => false,
    };
    if valid {
        Ok(())
    } else {
        Err(V1PostgresImportError::CorruptCheckpoint)
    }
}

fn validate_checkpoint_history(run: &ImportRun) -> Result<(), V1PostgresImportError> {
    const LEGAL_HISTORY: [V1PostgresImportCheckpoint; 4] = [
        V1PostgresImportCheckpoint::Staged,
        V1PostgresImportCheckpoint::Ready,
        V1PostgresImportCheckpoint::Bm25Rebuilt,
        V1PostgresImportCheckpoint::Complete,
    ];
    if run.checkpoint_history.is_empty()
        || run.checkpoint_history.len() > LEGAL_HISTORY.len()
        || run.checkpoint_history.as_slice() != &LEGAL_HISTORY[..run.checkpoint_history.len()]
        || run.checkpoint_history.last().copied() != Some(run.checkpoint)
    {
        Err(V1PostgresImportError::CorruptCheckpoint)
    } else {
        Ok(())
    }
}

fn validate_checkpoint_state(
    run: &ImportRun,
    state: Option<GenerationState>,
) -> Result<(), V1PostgresImportError> {
    validate_checkpoint_history(run)?;
    let valid = match state {
        None | Some(GenerationState::Retiring) => {
            run.checkpoint == V1PostgresImportCheckpoint::Complete
        }
        Some(GenerationState::Staging) => run.checkpoint == V1PostgresImportCheckpoint::Staged,
        Some(GenerationState::Ready) => matches!(
            run.checkpoint,
            V1PostgresImportCheckpoint::Staged
                | V1PostgresImportCheckpoint::Ready
                | V1PostgresImportCheckpoint::Bm25Rebuilt
        ),
        Some(GenerationState::Current | GenerationState::Superseded) => matches!(
            run.checkpoint,
            V1PostgresImportCheckpoint::Bm25Rebuilt | V1PostgresImportCheckpoint::Complete
        ),
        Some(GenerationState::Failed) => false,
    };
    if valid {
        Ok(())
    } else {
        Err(V1PostgresImportError::CorruptCheckpoint)
    }
}

async fn register_import_project(
    database: &CartographDatabase,
    request: &V1PostgresImportRequest,
    root_identity: &str,
) -> Result<ProjectId, V1PostgresImportError> {
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("register-project-begin"))?;
    crate::database::set_local_statement_timeout(
        &mut transaction,
        request.execution.statement_timeout,
    )
    .await
    .map_err(|()| database_error("register-project-statement-timeout"))?;
    let destination = crate::database::quoted_schema(&database.schema);
    let sql = format!(
        r#"INSERT INTO {destination}."projects" (root_identity, repository_fingerprint)
            VALUES ($1, $2)
            ON CONFLICT (root_identity) DO UPDATE
            SET root_identity = EXCLUDED.root_identity
            RETURNING project_id::text, repository_fingerprint"#
    );
    let row = query(AssertSqlSafe(sql))
        .bind(root_identity)
        .bind(request.source.revision.repository_fingerprint.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| database_error("register-project"))?
        .ok_or_else(|| database_error("register-project"))?;
    let repository_fingerprint =
        read_named_string(&row, "repository_fingerprint", "repository_fingerprint")?;
    if repository_fingerprint != request.source.revision.repository_fingerprint.as_str() {
        return Err(V1PostgresImportError::SourceFingerprintChanged);
    }
    let project_id = ProjectId::parse(&read_named_string(&row, "project_id", "project_id")?)
        .map_err(|_| database_error("register-project"))?;
    transaction
        .commit()
        .await
        .map_err(|_| database_error("register-project-commit"))?;
    Ok(project_id)
}

async fn recover_import_generation(
    context: &mut LeasedImportContext<'_>,
) -> Result<crate::ReadyGeneration, V1PostgresImportError> {
    let recovered = context
        .database
        .recover_generation_bounded(
            GenerationRecoveryRequest::new(
                &context.active.initialized.project_id,
                &context.active.initialized.run.generation_id,
            ),
            context.execution.statement_timeout,
        )
        .await
        .map_err(|_| database_error("recover-import-generation"))?
        .ok_or(V1PostgresImportError::CorruptCheckpoint)?;
    match (context.active.generation_state, recovered) {
        (GenerationState::Staging, RecoverableGeneration::Staged(staged)) => {
            let facts = context
                .active
                .initialized
                .analysis
                .facts
                .take()
                .ok_or(V1PostgresImportError::CorruptCheckpoint)?;
            let prepare = context.database.prepare_generation_bounded(
                GenerationContents::new(staged, facts),
                PrepareGenerationMutation::new(context.fence, context.execution.statement_timeout),
            );
            run_with_heartbeat(
                &mut HeartbeatContext {
                    database: context.database,
                    execution: context.execution,
                    lease: &mut *context.lease,
                },
                prepare,
            )
            .await?
            .map_err(|error| storage_mutation_error(error.error(), "prepare-generation"))
        }
        (GenerationState::Ready, RecoverableGeneration::Ready(ready)) => Ok(ready),
        (
            GenerationState::Staging | GenerationState::Ready,
            RecoverableGeneration::Staged(_) | RecoverableGeneration::Ready(_),
        )
        | (
            GenerationState::Current
            | GenerationState::Superseded
            | GenerationState::Failed
            | GenerationState::Retiring,
            _,
        ) => Err(V1PostgresImportError::CorruptCheckpoint),
    }
}

async fn run_with_heartbeat<Output, Operation>(
    context: &mut HeartbeatContext<'_>,
    operation: Operation,
) -> Result<Output, V1PostgresImportError>
where
    Operation: Future<Output = Output>,
{
    let heartbeat_interval = context.execution.lease_duration / 3;
    let heartbeat_timeout = context.execution.statement_timeout.min(heartbeat_interval);
    tokio::pin!(operation);
    loop {
        tokio::select! {
            output = &mut operation => return Ok(output),
            () = tokio::time::sleep(heartbeat_interval) => {
                context.database
                    .heartbeat_lease_bounded(context.lease, heartbeat_timeout)
                    .await
                    .map_err(|error| lease_heartbeat_error(&error))?;
            }
        }
    }
}

async fn analyze_source(
    database: &CartographDatabase,
    request: &V1PostgresImportRequest,
) -> Result<V1Analysis, V1PostgresImportError> {
    validate_request(database, request)?;
    let canonical_root = request
        .source
        .project_root
        .canonicalize()
        .map_err(|_| invalid_source("project_root"))?;
    if !canonical_root.is_dir() {
        return Err(invalid_source("project_root"));
    }
    let deadline = Instant::now()
        .checked_add(request.execution.statement_timeout)
        .ok_or(V1PostgresImportError::InvalidInput {
            field: "statement_timeout",
        })?;
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("source-snapshot-begin"))?;
    if query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *transaction)
        .await
        .is_err()
    {
        let _ = transaction.rollback().await;
        return Err(database_error("source-snapshot-isolation"));
    }
    if crate::database::set_local_statement_timeout(
        &mut transaction,
        request.execution.statement_timeout,
    )
    .await
    .is_err()
    {
        let _ = transaction.rollback().await;
        return Err(V1PostgresImportError::InvalidInput {
            field: "statement_timeout",
        });
    }
    let result = tokio::time::timeout_at(
        deadline,
        read_source_snapshot(request, &canonical_root, &mut transaction),
    )
    .await
    .map_err(|_| V1PostgresImportError::AnalysisDeadline)?;
    match result {
        Ok(source) => {
            tokio::time::timeout_at(deadline, transaction.commit())
                .await
                .map_err(|_| V1PostgresImportError::AnalysisDeadline)?
                .map_err(|_| database_error("source-snapshot-commit"))?;
            let owned_request = request.clone();
            tokio::task::spawn_blocking(move || {
                finalize_source_analysis(&owned_request, source, deadline)
            })
            .await
            .map_err(|_| database_error("source-analysis-worker"))?
        }
        Err(error) => {
            transaction
                .rollback()
                .await
                .map_err(|_| database_error("source-snapshot-rollback"))?;
            Err(error)
        }
    }
}

async fn read_source_snapshot(
    request: &V1PostgresImportRequest,
    canonical_root: &Path,
    connection: &mut sqlx_postgres::PgConnection,
) -> Result<SourceSnapshot, V1PostgresImportError> {
    let preflight = preflight_source(connection, request).await?;
    load_source(
        connection,
        SourceSnapshotRead {
            request,
            root: canonical_root,
            preflight: &preflight,
        },
    )
    .await
}

fn finalize_source_analysis(
    request: &V1PostgresImportRequest,
    source: SourceSnapshot,
    deadline: Instant,
) -> Result<V1Analysis, V1PostgresImportError> {
    ensure_analysis_active(deadline)?;
    let admission = admit_mapping(&source, request.limits)?;
    let mut raw_facts = map_source_facts(&source, deadline)?;
    apply_import_centrality(&mut raw_facts, deadline)?;
    let facts = validate_import_facts(
        raw_facts,
        &ImportValidation {
            limits: request.limits.validation,
            source_retained_bytes: admission.source_retained_bytes,
            deadline,
        },
    )?;
    let counts = canonical_counts(&facts)?;
    Ok(V1Analysis {
        report: V1PostgresDryRunReport {
            source_revision: source.source_revision,
            source_fingerprint: source.fingerprint,
            content_digest: facts.digest().clone(),
            counts,
            source_bytes: source.source_bytes,
        },
        facts: Some(facts),
    })
}

fn apply_import_centrality(
    raw_facts: &mut GenerationFacts,
    deadline: Instant,
) -> Result<(), V1PostgresImportError> {
    apply_page_rank(raw_facts, || Instant::now() >= deadline).map_err(|error| match error {
        crate::PageRankError::Cancelled => V1PostgresImportError::AnalysisDeadline,
        crate::PageRankError::WorkerFailed | crate::PageRankError::NumericOverflow => {
            V1PostgresImportError::CanonicalValidation
        }
    })?;
    apply_sampled_betweenness(raw_facts, || Instant::now() >= deadline).map_err(
        |error| match error {
            crate::BetweennessError::Cancelled => V1PostgresImportError::AnalysisDeadline,
            crate::BetweennessError::WorkerFailed | crate::BetweennessError::NumericOverflow => {
                V1PostgresImportError::CanonicalValidation
            }
        },
    )?;
    Ok(())
}

struct ImportValidation {
    limits: GenerationValidationLimits,
    source_retained_bytes: u64,
    deadline: Instant,
}

fn validate_import_facts(
    raw_facts: GenerationFacts,
    validation: &ImportValidation,
) -> Result<CanonicalGenerationFacts, V1PostgresImportError> {
    let validation_working_bytes = validation
        .limits
        .maximum_working_bytes()
        .checked_sub(validation.source_retained_bytes)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let validation_limits = GenerationValidationLimits::new(
        validation
            .limits
            .maximum_output_bytes()
            .min(validation_working_bytes),
        validation_working_bytes,
    )
    .map_err(|_| V1PostgresImportError::SourceLimit)?;
    crate::ingest::validate_generation_facts_for_v1_import(raw_facts, validation_limits, || {
        Instant::now() >= validation.deadline
    })
    .map(|(facts, _)| facts)
    .map_err(|error| match error {
        crate::GenerationValidationError::Cancelled => V1PostgresImportError::AnalysisDeadline,
        _ => V1PostgresImportError::CanonicalValidation,
    })
}

struct MappingAdmission {
    source_retained_bytes: u64,
}

fn admit_mapping(
    source: &SourceSnapshot,
    limits: V1PostgresImportLimits,
) -> Result<MappingAdmission, V1PostgresImportError> {
    let source_retained_bytes = source_retained_bytes(source)?;
    let (derived_rows, derived_bytes) = derived_mapping_bounds(source)?;
    let source_rows = usize_to_u64(source.files.len())?
        .checked_add(usize_to_u64(source.nodes.len())?)
        .and_then(|rows| rows.checked_add(usize_to_u64(source.edges.len()).ok()?))
        .and_then(|rows| rows.checked_add(usize_to_u64(source.references.len()).ok()?))
        .ok_or(V1PostgresImportError::SourceLimit)?;
    if source_rows > limits.maximum_rows
        || derived_rows > MAXIMUM_IMPORT_ROWS
        || source_retained_bytes
            .checked_add(derived_bytes)
            .is_none_or(|bytes| bytes > limits.validation.maximum_working_bytes())
    {
        return Err(V1PostgresImportError::SourceLimit);
    }
    Ok(MappingAdmission {
        source_retained_bytes,
    })
}

fn source_retained_bytes(source: &SourceSnapshot) -> Result<u64, V1PostgresImportError> {
    let mut total = usize_to_u64(size_of::<SourceSnapshot>())?;
    for (path, file) in &source.files {
        charge_source_file(&mut total, path, file)?;
    }
    for node in &source.nodes {
        charge_source_node(&mut total, node)?;
    }
    for edge in &source.edges {
        charge_source_edge(&mut total, edge)?;
    }
    for reference in &source.references {
        charge_source_reference(&mut total, reference)?;
    }
    Ok(total)
}

fn charge_source_file(
    total: &mut u64,
    path: &String,
    file: &SourceFile,
) -> Result<(), V1PostgresImportError> {
    charge_bytes(total, SOURCE_ROW_ALLOCATION_ALLOWANCE)?;
    charge_capacities(
        total,
        [
            path.capacity(),
            file.language.capacity(),
            file.source.capacity(),
        ],
    )?;
    charge_vector_capacity(total, file.line_starts.capacity(), size_of::<u32>())
}

fn charge_source_node(total: &mut u64, node: &SourceNode) -> Result<(), V1PostgresImportError> {
    charge_bytes(total, SOURCE_ROW_ALLOCATION_ALLOWANCE)?;
    charge_capacities(
        total,
        [
            node.legacy_id.capacity(),
            node.name.capacity(),
            node.kind.capacity(),
            node.qualified_name.capacity(),
            node.file_path.capacity(),
            node.language.capacity(),
            node.docstring.as_ref().map_or(0, String::capacity),
            node.signature.as_ref().map_or(0, String::capacity),
            node.body_hash.capacity(),
            node.visibility.as_ref().map_or(0, String::capacity),
        ],
    )
}

fn charge_source_edge(total: &mut u64, edge: &SourceEdge) -> Result<(), V1PostgresImportError> {
    charge_bytes(total, SOURCE_ROW_ALLOCATION_ALLOWANCE)?;
    charge_capacities(
        total,
        [
            edge.source.capacity(),
            edge.target.capacity(),
            edge.kind.capacity(),
            edge.confidence.as_ref().map_or(0, String::capacity),
            edge.metadata
                .provenance
                .as_ref()
                .map_or(0, String::capacity),
            edge.metadata
                .def_use_name
                .as_ref()
                .map_or(0, String::capacity),
        ],
    )?;
    charge_vector_capacity(
        total,
        edge.metadata.extra_lines.capacity(),
        size_of::<u32>(),
    )?;
    charge_vector_capacity(
        total,
        edge.metadata.def_use_lines.capacity(),
        size_of::<u32>(),
    )
}

fn charge_source_reference(
    total: &mut u64,
    reference: &SourceReference,
) -> Result<(), V1PostgresImportError> {
    charge_bytes(total, SOURCE_ROW_ALLOCATION_ALLOWANCE)?;
    charge_capacities(
        total,
        [
            reference.from_node_id.capacity(),
            reference.reference_name.capacity(),
            reference.reference_kind.capacity(),
            reference.file_path.capacity(),
            reference.language.capacity(),
        ],
    )?;
    charge_capacities(total, reference.candidates.iter().map(String::capacity))?;
    charge_vector_capacity(total, reference.candidates.capacity(), size_of::<String>())?;
    charge_vector_capacity(total, reference.extra_lines.capacity(), size_of::<u32>())
}

fn charge_capacities(
    total: &mut u64,
    capacities: impl IntoIterator<Item = usize>,
) -> Result<(), V1PostgresImportError> {
    for capacity in capacities {
        charge_bytes(total, usize_to_u64(capacity)?)?;
    }
    Ok(())
}

fn charge_vector_capacity(
    total: &mut u64,
    capacity: usize,
    element_bytes: usize,
) -> Result<(), V1PostgresImportError> {
    let bytes = usize_to_u64(capacity)?
        .checked_mul(usize_to_u64(element_bytes)?)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    charge_bytes(total, bytes)
}

fn derived_mapping_bounds(source: &SourceSnapshot) -> Result<(u64, u64), V1PostgresImportError> {
    let derived_rows = derived_mapping_rows(source)?;
    let dynamic_bytes = derived_mapping_dynamic_bytes(source)?;
    let fixed_bytes = derived_rows
        .checked_mul(DERIVED_ROW_ALLOCATION_ALLOWANCE)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    Ok((
        derived_rows,
        fixed_bytes
            .checked_add(dynamic_bytes)
            .ok_or(V1PostgresImportError::SourceLimit)?,
    ))
}

fn derived_mapping_rows(source: &SourceSnapshot) -> Result<u64, V1PostgresImportError> {
    let file_rows = usize_to_u64(source.files.len())?;
    let symbol_rows = usize_to_u64(source.nodes.len())?;
    let edge_rows = usize_to_u64(source.edges.len())?;
    let document_rows = file_rows
        .checked_add(symbol_rows)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let unresolved_reference_rows = source
        .references
        .iter()
        .try_fold(0_u64, |total, reference| {
            total.checked_add(
                PRIMARY_REFERENCE_ROW_COUNT
                    .saturating_add(usize_to_u64(reference.extra_lines.len()).ok()?),
            )
        })
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let resolved_reference_rows = source
        .edges
        .iter()
        .try_fold(0_u64, |total, edge| {
            let rows = edge_reference_rows(edge);
            total.checked_add(usize_to_u64(rows).ok()?)
        })
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let reference_rows = unresolved_reference_rows
        .checked_add(resolved_reference_rows)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    file_rows
        .checked_add(symbol_rows)
        .and_then(|rows| rows.checked_add(edge_rows))
        .and_then(|rows| rows.checked_add(reference_rows))
        .and_then(|rows| rows.checked_add(document_rows))
        .ok_or(V1PostgresImportError::SourceLimit)
}

fn derived_mapping_dynamic_bytes(source: &SourceSnapshot) -> Result<u64, V1PostgresImportError> {
    let mut dynamic_bytes = 0_u64;
    for (path, file) in &source.files {
        charge_file_mapping_bytes(&mut dynamic_bytes, path, file)?;
    }
    for node in &source.nodes {
        charge_node_mapping_bytes(&mut dynamic_bytes, node)?;
    }
    for reference in &source.references {
        charge_unresolved_reference_mapping_bytes(&mut dynamic_bytes, reference)?;
    }
    for edge in &source.edges {
        charge_edge_reference_mapping_bytes(&mut dynamic_bytes, edge)?;
    }
    Ok(dynamic_bytes)
}

fn charge_file_mapping_bytes(
    total: &mut u64,
    path: &str,
    file: &SourceFile,
) -> Result<(), V1PostgresImportError> {
    charge_bytes(
        total,
        usize_to_u64(path.len().saturating_mul(FACT_MAPPING_FILE_PATH_COPIES))?,
    )?;
    charge_bytes(
        total,
        usize_to_u64(
            file.language
                .len()
                .saturating_mul(FACT_MAPPING_FILE_LANGUAGE_COPIES),
        )?,
    )
}

fn charge_node_mapping_bytes(
    total: &mut u64,
    node: &SourceNode,
) -> Result<(), V1PostgresImportError> {
    for bytes in [
        node.legacy_id
            .len()
            .saturating_mul(FACT_MAPPING_LEGACY_ID_COPIES)
            .saturating_add(
                FACT_MAPPING_BTREE_ENTRY_ALLOWANCE.saturating_mul(FACT_MAPPING_LEGACY_ID_COPIES),
            ),
        node.kind.len(),
        node.qualified_name
            .len()
            .saturating_mul(FACT_MAPPING_QUALIFIED_NAME_COPIES),
        node.signature
            .as_ref()
            .map_or(0, String::len)
            .saturating_mul(FACT_MAPPING_SIGNATURE_COPIES),
        node.file_path.len(),
        node.language.len(),
        node.docstring.as_ref().map_or(0, String::len),
        node.name.len(),
        node.visibility.as_ref().map_or(0, String::len),
    ] {
        charge_bytes(total, usize_to_u64(bytes)?)?;
    }
    Ok(())
}

fn charge_unresolved_reference_mapping_bytes(
    total: &mut u64,
    reference: &SourceReference,
) -> Result<(), V1PostgresImportError> {
    let rows = PRIMARY_REFERENCE_ROW_COUNT
        .checked_add(usize_to_u64(reference.extra_lines.len())?)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let per_row = reference
        .reference_name
        .len()
        .saturating_add(reference.reference_kind.len())
        .saturating_add(REFERENCE_DYNAMIC_ROW_ALLOWANCE);
    charge_mapping_rows(total, rows, per_row)
}

fn charge_edge_reference_mapping_bytes(
    total: &mut u64,
    edge: &SourceEdge,
) -> Result<(), V1PostgresImportError> {
    let reference_name_bytes = edge
        .metadata
        .def_use_name
        .as_ref()
        .map_or(MAXIMUM_LEGACY_NAME_BYTES, String::len);
    let per_row = reference_name_bytes
        .saturating_add(edge.kind.len())
        .saturating_add(REFERENCE_DYNAMIC_ROW_ALLOWANCE);
    charge_mapping_rows(total, usize_to_u64(edge_reference_rows(edge))?, per_row)
}

fn charge_mapping_rows(
    total: &mut u64,
    rows: u64,
    bytes_per_row: usize,
) -> Result<(), V1PostgresImportError> {
    charge_bytes(
        total,
        rows.checked_mul(usize_to_u64(bytes_per_row)?)
            .ok_or(V1PostgresImportError::SourceLimit)?,
    )
}

fn edge_reference_rows(edge: &SourceEdge) -> usize {
    if edge.kind == "def_use" {
        edge.metadata
            .def_use_name
            .as_ref()
            .map_or(0, |_| edge.metadata.def_use_lines.len())
    } else {
        usize::from(edge.line.is_some()).saturating_add(edge.metadata.extra_lines.len())
    }
}

fn charge_bytes(total: &mut u64, bytes: u64) -> Result<(), V1PostgresImportError> {
    *total = total
        .checked_add(bytes)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    Ok(())
}

fn ensure_analysis_active(deadline: Instant) -> Result<(), V1PostgresImportError> {
    if Instant::now() >= deadline {
        Err(V1PostgresImportError::AnalysisDeadline)
    } else {
        Ok(())
    }
}

fn validate_request(
    database: &CartographDatabase,
    request: &V1PostgresImportRequest,
) -> Result<(), V1PostgresImportError> {
    if request.source.source_schema == database.schema {
        return Err(V1PostgresImportError::SameSchema);
    }
    if request.execution.lease_duration.is_zero() || request.execution.statement_timeout.is_zero() {
        return Err(V1PostgresImportError::InvalidInput {
            field: "import_deadline",
        });
    }
    Ok(())
}

async fn preflight_source(
    connection: &mut sqlx_postgres::PgConnection,
    request: &V1PostgresImportRequest,
) -> Result<SourcePreflight, V1PostgresImportError> {
    let supported_languages = native_import_languages();
    let row = query(AssertSqlSafe(source_preflight_sql(request)))
        .bind(&supported_languages)
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| V1PostgresImportError::UnsupportedSourceSchema)?;
    validate_preflight_schema(&row)?;
    let evidence = decode_preflight_evidence(&row)?;
    validate_preflight_limits(&row, request, &evidence)?;
    let actual_checkout_byte_budget = preflight_memory_admission(PreflightMemoryInput {
        counts: evidence.counts,
        source_bytes: evidence.source_bytes,
        source_metadata_bytes: evidence.source_metadata_bytes,
        source_json_bytes: evidence.source_json_bytes,
        maximum_source_bytes: request.limits.maximum_source_bytes,
        maximum_working_bytes: request.limits.validation.maximum_working_bytes(),
    })?;
    validate_preflight_relations(&row)?;
    Ok(SourcePreflight {
        counts: evidence.counts,
        source_bytes: evidence.source_bytes,
        actual_checkout_byte_budget,
    })
}

fn source_preflight_sql(request: &V1PostgresImportRequest) -> String {
    let source = crate::database::quoted_schema(&request.source.source_schema);
    let sql = SOURCE_PREFLIGHT_SQL_TEMPLATE
        .replace("{source}", &source)
        .replace("{version}", &V1_SCHEMA_VERSION.to_string());
    replace_preflight_field_limits(&sql, request)
}

fn native_import_languages() -> Vec<String> {
    SourceLanguage::ALL
        .into_iter()
        .filter(|language| language.is_native_indexable())
        .map(|language| language.as_str().to_owned())
        .collect()
}

fn replace_preflight_field_limits(sql: &str, request: &V1PostgresImportRequest) -> String {
    sql.replace("{max_path}", &MAXIMUM_LEGACY_PATH_BYTES.to_string())
        .replace(
            "{max_content_hash}",
            &MAXIMUM_LEGACY_CONTENT_HASH_BYTES.to_string(),
        )
        .replace("{max_language}", &MAXIMUM_LEGACY_LANGUAGE_BYTES.to_string())
        .replace("{max_errors}", &MAXIMUM_LEGACY_ERRORS_BYTES.to_string())
        .replace(
            "{max_file_bytes}",
            &request
                .limits
                .maximum_source_bytes
                .min(MAXIMUM_IMPORT_FILE_BYTES)
                .to_string(),
        )
        .replace("{max_id}", &MAXIMUM_LEGACY_ID_BYTES.to_string())
        .replace("{max_name}", &MAXIMUM_LEGACY_NAME_BYTES.to_string())
        .replace("{max_kind}", &MAXIMUM_LEGACY_KIND_BYTES.to_string())
        .replace(
            "{max_qualified_name}",
            &MAXIMUM_LEGACY_QUALIFIED_NAME_BYTES.to_string(),
        )
        .replace(
            "{max_docstring}",
            &MAXIMUM_LEGACY_DOCSTRING_BYTES.to_string(),
        )
        .replace(
            "{max_signature}",
            &MAXIMUM_LEGACY_SIGNATURE_BYTES.to_string(),
        )
        .replace(
            "{max_body_hash}",
            &MAXIMUM_LEGACY_BODY_HASH_BYTES.to_string(),
        )
        .replace(
            "{max_visibility}",
            &MAXIMUM_LEGACY_VISIBILITY_BYTES.to_string(),
        )
        .replace("{max_metadata}", &MAXIMUM_LEGACY_METADATA_BYTES.to_string())
}

fn validate_preflight_schema(row: &sqlx_postgres::PgRow) -> Result<(), V1PostgresImportError> {
    let version = read_named_i64(row, "schema_version", "schema_version")?;
    if version != V1_SCHEMA_VERSION
        || !read_named_bool(row, "valid_schema_history", "schema_history")?
    {
        Err(V1PostgresImportError::UnsupportedSourceSchema)
    } else {
        Ok(())
    }
}

fn decode_preflight_evidence(
    row: &sqlx_postgres::PgRow,
) -> Result<SourcePreflightEvidence, V1PostgresImportError> {
    let counts = V1PostgresImportCounts {
        files: read_named_u64(row, "file_count", "file_count")?,
        symbols: read_named_u64(row, "symbol_count", "symbol_count")?,
        edges: read_named_u64(row, "edge_count", "edge_count")?,
        edge_sites: read_named_u64(row, "edge_count", "edge_count")?,
        references: read_named_u64(row, "reference_count", "reference_count")?,
        reference_sites: read_named_u64(row, "reference_count", "reference_count")?,
        documents: 0,
    };
    Ok(SourcePreflightEvidence {
        counts,
        source_bytes: read_named_u64(row, "source_bytes", "source_bytes")?,
        source_metadata_bytes: read_named_u64(
            row,
            "source_metadata_bytes",
            "source_metadata_bytes",
        )?,
        source_json_bytes: read_named_u64(row, "source_json_bytes", "source_json_bytes")?,
    })
}

fn validate_preflight_limits(
    row: &sqlx_postgres::PgRow,
    request: &V1PostgresImportRequest,
    evidence: &SourcePreflightEvidence,
) -> Result<(), V1PostgresImportError> {
    let oversized = read_named_u64(row, "oversized_field_count", "source_field_limit")? != 0;
    let too_many_rows = evidence
        .counts
        .total()
        .is_none_or(|total| total > request.limits.maximum_rows);
    let too_many_bytes = evidence
        .source_bytes
        .checked_add(evidence.source_metadata_bytes)
        .is_none_or(|bytes| bytes > request.limits.maximum_source_bytes);
    if oversized || too_many_rows || too_many_bytes {
        Err(V1PostgresImportError::SourceLimit)
    } else {
        Ok(())
    }
}

fn validate_preflight_relations(row: &sqlx_postgres::PgRow) -> Result<(), V1PostgresImportError> {
    for (column, field) in [
        ("needs_reextract_count", "needs_reextract"),
        ("orphan_node_count", "node_file_relation"),
        ("orphan_edge_count", "edge_symbol_relation"),
        ("orphan_reference_count", "reference_relation"),
        ("node_count_mismatch", "file_node_count"),
        ("unsupported_language_count", "source_language"),
    ] {
        if read_named_u64(row, column, field)? != 0 {
            return Err(invalid_source(field));
        }
    }
    Ok(())
}

fn preflight_memory_admission(input: PreflightMemoryInput) -> Result<u64, V1PostgresImportError> {
    let source_rows = input
        .counts
        .total()
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let minimum_document_rows = input
        .counts
        .files
        .checked_add(input.counts.symbols)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let minimum_derived_rows = source_rows
        .checked_add(minimum_document_rows)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let source_row_floor = source_rows
        .checked_mul(SOURCE_ROW_ALLOCATION_ALLOWANCE)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    // JSON is parsed by bounded streaming visitors without a `Value` tree. Sixteen times the
    // raw JSON covers the retained raw bytes, direct typed-vector backing, owned string bytes,
    // vector growth slack, and one deserializer scratch string at the smallest valid shapes.
    let expanded_json_floor = input
        .source_json_bytes
        .checked_mul(SOURCE_JSON_EXPANSION_FACTOR)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let non_json_metadata = input
        .source_metadata_bytes
        .checked_sub(input.source_json_bytes)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let non_source_floor = non_json_metadata
        .checked_add(expanded_json_floor)
        .and_then(|bytes| bytes.checked_add(source_row_floor))
        .and_then(|bytes| {
            bytes.checked_add(minimum_derived_rows.checked_mul(DERIVED_ROW_ALLOCATION_ALLOWANCE)?)
        })
        .and_then(|bytes| {
            bytes.checked_add(if input.counts.symbols == 0 {
                0
            } else {
                V1_BODY_HASH_TRANSIENT_BYTES
            })
        })
        .and_then(|bytes| {
            bytes.checked_add(if input.counts.files == 0 {
                0
            } else {
                SOURCE_PATH_TRANSIENT_BYTES
            })
        })
        .and_then(|bytes| {
            bytes.checked_add(if source_rows == 0 {
                0
            } else {
                SOURCE_ROW_DECODE_TRANSIENT_BYTES
            })
        })
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let source_policy_budget = input
        .maximum_source_bytes
        .checked_sub(input.source_metadata_bytes)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let working_variable_budget = input
        .maximum_working_bytes
        .checked_sub(non_source_floor)
        .and_then(|bytes| bytes.checked_sub(SOURCE_LINE_OFFSET_BYTES))
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let working_checkout_budget = working_variable_budget / (1 + SOURCE_LINE_OFFSET_BYTES);
    let actual_checkout_byte_budget = source_policy_budget.min(working_checkout_budget);
    if input.source_bytes > actual_checkout_byte_budget {
        return Err(V1PostgresImportError::SourceLimit);
    }
    Ok(actual_checkout_byte_budget)
}

struct SourceReadPlan<'a> {
    quoted_schema: String,
    limit: i64,
    root: &'a Path,
    maximum_source_bytes: u64,
    expected: &'a SourcePreflight,
}

struct SourceSnapshotRead<'a> {
    request: &'a V1PostgresImportRequest,
    root: &'a Path,
    preflight: &'a SourcePreflight,
}

struct SourceReadState<'a, 'b> {
    plan: &'a SourceReadPlan<'b>,
    fingerprint: &'a mut blake3::Hasher,
}

struct DecodedSourceFile {
    key: String,
    file: SourceFile,
    stored_hash: String,
    raw_errors: Option<String>,
    stored_size: u64,
    actual_size: u64,
}

#[derive(Clone, Copy)]
struct Utf16Position {
    line_start: usize,
    line_end: usize,
    column: usize,
}

async fn load_source(
    connection: &mut sqlx_postgres::PgConnection,
    input: SourceSnapshotRead<'_>,
) -> Result<SourceSnapshot, V1PostgresImportError> {
    let plan = SourceReadPlan {
        quoted_schema: crate::database::quoted_schema(&input.request.source.source_schema),
        limit: i64::try_from(input.request.limits.maximum_rows)
            .map_err(|_| V1PostgresImportError::SourceLimit)?,
        root: input.root,
        maximum_source_bytes: input.preflight.actual_checkout_byte_budget,
        expected: input.preflight,
    };
    let mut fingerprint = source_fingerprint_hasher(input.request);
    let mut state = SourceReadState {
        plan: &plan,
        fingerprint: &mut fingerprint,
    };
    let (files, source_bytes) = load_source_files(connection, &mut state).await?;
    let source_revision = require_source_manifest(
        &files,
        &input.request.source.revision.expected_source_manifest,
    )?;
    hash_field(state.fingerprint, source_revision.as_str().as_bytes());
    let nodes = load_source_nodes(connection, &mut state, &files).await?;
    let edges = load_source_edges(connection, &mut state).await?;
    let references = load_source_references(connection, &mut state).await?;
    Ok(SourceSnapshot {
        files,
        nodes,
        edges,
        references,
        source_revision,
        fingerprint: ContentDigest::from_bytes(*fingerprint.finalize().as_bytes()),
        source_bytes,
    })
}

fn require_source_manifest(
    files: &BTreeMap<String, SourceFile>,
    expected: &ContentDigest,
) -> Result<ContentDigest, V1PostgresImportError> {
    let mut manifest = SourceManifestDigestBuilder::new(files.len())
        .map_err(|_| V1PostgresImportError::SourceLimit)?;
    for (path, file) in files {
        let normalized =
            NormalizedPath::parse(path).map_err(|_| invalid_source("source_manifest_path"))?;
        manifest
            .push(&normalized, &file.content_hash)
            .map_err(|_| invalid_source("source_manifest_order"))?;
    }
    let actual = manifest
        .finish()
        .map_err(|_| invalid_source("source_manifest_count"))?;
    if expected == &actual {
        Ok(actual)
    } else {
        Err(V1PostgresImportError::SourceManifestMismatch)
    }
}

fn source_fingerprint_hasher(request: &V1PostgresImportRequest) -> blake3::Hasher {
    let mut fingerprint = blake3::Hasher::new();
    for value in [
        b"cartograph-v1.1.33-postgres".as_slice(),
        request.source.source_schema.as_str().as_bytes(),
        request
            .source
            .revision
            .repository_fingerprint
            .as_str()
            .as_bytes(),
    ] {
        hash_field(&mut fingerprint, value);
    }
    fingerprint
}

async fn load_source_files(
    connection: &mut sqlx_postgres::PgConnection,
    state: &mut SourceReadState<'_, '_>,
) -> Result<(BTreeMap<String, SourceFile>, u64), V1PostgresImportError> {
    let sql = format!(
        r#"SELECT path, content_hash, language, size::bigint AS size,
                  errors, is_test::bigint AS is_test
            FROM {}."files" ORDER BY path LIMIT $1"#,
        state.plan.quoted_schema
    );
    let mut rows = query(AssertSqlSafe(sql))
        .bind(state.plan.limit)
        .fetch(&mut *connection);
    let mut files = BTreeMap::new();
    let mut observed_bytes = 0_u64;
    let mut observed_stored_bytes = 0_u64;
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|_| database_error("read-source-files"))?
    {
        let remaining_source_bytes = state
            .plan
            .maximum_source_bytes
            .checked_sub(observed_bytes)
            .ok_or(V1PostgresImportError::SourceLimit)?;
        let decoded = decode_source_file(&row, state.plan, remaining_source_bytes).await?;
        observed_bytes = observed_bytes
            .checked_add(decoded.actual_size)
            .ok_or(V1PostgresImportError::SourceLimit)?;
        observed_stored_bytes = observed_stored_bytes
            .checked_add(decoded.stored_size)
            .ok_or(V1PostgresImportError::SourceLimit)?;
        if observed_bytes > state.plan.maximum_source_bytes {
            return Err(V1PostgresImportError::SourceLimit);
        }
        hash_source_file(state.fingerprint, &decoded);
        if files.insert(decoded.key, decoded.file).is_some() {
            return Err(invalid_source("duplicate_file_path"));
        }
    }
    if observed_stored_bytes != state.plan.expected.source_bytes
        || usize_to_u64(files.len())? != state.plan.expected.counts.files
    {
        return Err(invalid_source("file_counts"));
    }
    Ok((files, observed_bytes))
}

async fn decode_source_file(
    row: &sqlx_postgres::PgRow,
    plan: &SourceReadPlan<'_>,
    remaining_source_bytes: u64,
) -> Result<DecodedSourceFile, V1PostgresImportError> {
    let path = read_named_string(row, "path", "file_path")?;
    let stored_hash = read_named_string(row, "content_hash", "content_hash")?;
    let language = read_supported_language(row, "language", "language")?;
    let stored_size = read_named_u64(row, "size", "file_size")?;
    let raw_errors = read_named_optional_string(row, "errors", "file_errors")?;
    let has_extraction_errors = has_valid_extraction_errors(raw_errors.as_deref());
    let is_test = read_named_i64(row, "is_test", "file_is_test")?;
    if !matches!(is_test, 0 | 1) {
        return Err(invalid_source("file_is_test"));
    }
    let key = NormalizedPath::parse(&path)
        .map_err(|_| invalid_source("file_path"))?
        .into_string();
    let canonical = tokio::fs::canonicalize(plan.root.join(&key))
        .await
        .map_err(|_| invalid_source("source_file"))?;
    if !canonical.starts_with(plan.root) || !canonical.is_file() {
        return Err(invalid_source("source_file"));
    }
    let file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|_| invalid_source("source_file"))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| invalid_source("source_file"))?;
    let scip_placeholder = is_scip_placeholder(&stored_hash, stored_size, &key);
    let actual_size = metadata.len();
    if !metadata.is_file() || (!scip_placeholder && actual_size != stored_size) {
        return Err(invalid_source("file_size"));
    }
    if actual_size > plan.maximum_source_bytes
        || actual_size > remaining_source_bytes
        || actual_size > MAXIMUM_IMPORT_FILE_BYTES
    {
        return Err(invalid_source("file_size"));
    }
    let read_limit = actual_size
        .checked_add(1)
        .ok_or(V1PostgresImportError::SourceLimit)?;
    let capacity = usize::try_from(actual_size).map_err(|_| V1PostgresImportError::SourceLimit)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| V1PostgresImportError::SourceLimit)?;
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| invalid_source("source_file"))?;
    if usize_to_u64(bytes.len())? != actual_size {
        return Err(invalid_source("file_size"));
    }
    let source = String::from_utf8(bytes).map_err(|_| invalid_source("source_utf8"))?;
    if !source_matches_native_language(&key, &source, &language) {
        return Err(invalid_source("file_language"));
    }
    if !scip_placeholder && !v1_content_hash_matches(&source, &stored_hash) {
        return Err(V1PostgresImportError::ContentHashMismatch);
    }
    let content_hash = ContentDigest::from_bytes(*blake3::hash(source.as_bytes()).as_bytes());
    Ok(DecodedSourceFile {
        key,
        file: SourceFile {
            language,
            line_starts: line_starts(&source)?,
            source,
            content_hash,
            has_extraction_errors,
            is_test: is_test == 1,
        },
        stored_hash,
        raw_errors,
        stored_size,
        actual_size,
    })
}

fn source_matches_native_language(path: &str, source: &str, language: &str) -> bool {
    SourceLanguage::is_native_candidate_path(path)
        && SourceLanguage::for_normalized_path_with_source(path, source)
            .is_some_and(|detected| detected.as_str() == language)
}

fn v1_content_hash_matches(source: &str, stored_hash: &str) -> bool {
    let stored_hash = stored_hash.to_ascii_lowercase();
    sha256_hex(source.as_bytes()) == stored_hash
        || sha256_hex(source.strip_prefix('\u{feff}').unwrap_or(source).as_bytes()) == stored_hash
}

fn is_scip_placeholder(stored_hash: &str, stored_size: u64, normalized_path: &str) -> bool {
    let expected = sha256_hex(normalized_path.as_bytes());
    stored_size == 0 && stored_hash == format!("scip:{}", &expected[..24])
}

fn hash_source_file(hasher: &mut blake3::Hasher, decoded: &DecodedSourceFile) {
    for value in [
        decoded.key.as_bytes(),
        decoded.stored_hash.as_bytes(),
        decoded.file.language.as_bytes(),
        decoded.file.content_hash.as_str().as_bytes(),
    ] {
        hash_field(hasher, value);
    }
    hash_field(hasher, &decoded.stored_size.to_be_bytes());
    hash_field(hasher, &decoded.actual_size.to_be_bytes());
    hash_optional_field(hasher, decoded.raw_errors.as_deref());
    hash_field(hasher, &[u8::from(decoded.file.is_test)]);
}

async fn load_source_nodes(
    connection: &mut sqlx_postgres::PgConnection,
    state: &mut SourceReadState<'_, '_>,
    files: &BTreeMap<String, SourceFile>,
) -> Result<Vec<SourceNode>, V1PostgresImportError> {
    let sql = format!(
        r#"SELECT id, name, kind, qualified_name, file_path, language,
                  start_line, end_line, start_column, end_column,
                  docstring, signature, body_hash, visibility,
                  is_exported, is_default_export, is_async, is_static
            FROM {}."nodes" ORDER BY id LIMIT $1"#,
        state.plan.quoted_schema
    );
    let mut rows = query(AssertSqlSafe(sql))
        .bind(state.plan.limit)
        .fetch(&mut *connection);
    let mut nodes = Vec::new();
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|_| database_error("read-source-nodes"))?
    {
        let node = decode_source_node(&row)?;
        validate_v1_body_hash(&node, files)?;
        hash_node(state.fingerprint, &node);
        nodes.push(node);
    }
    require_source_count(
        nodes.len(),
        state.plan.expected.counts.symbols,
        "symbol_counts",
    )?;
    Ok(nodes)
}

fn decode_source_node(row: &sqlx_postgres::PgRow) -> Result<SourceNode, V1PostgresImportError> {
    let file_path = read_named_string(row, "file_path", "node_file_path")?;
    let kind = read_named_string(row, "kind", "node_kind")?;
    Ok(SourceNode {
        legacy_id: read_named_string(row, "id", "node_id")?,
        name: read_named_string(row, "name", "node_name")?,
        kind: kind.clone(),
        qualified_name: read_named_string(row, "qualified_name", "qualified_name")?,
        file_path: NormalizedPath::parse(&file_path)
            .map_err(|_| invalid_source("node_file_path"))?
            .into_string(),
        language: read_supported_language(row, "language", "node_language")?,
        start_line: read_named_u32(row, "start_line", "start_line")?,
        end_line: read_named_u32(row, "end_line", "end_line")?,
        start_column: normalize_legacy_start_column(
            row.try_get::<i32, _>("start_column")
                .map_err(|_| invalid_source("start_column"))?,
            &kind,
        )?,
        end_column: read_named_u32(row, "end_column", "end_column")?,
        docstring: read_named_optional_string(row, "docstring", "docstring")?,
        signature: read_named_optional_string(row, "signature", "signature")?,
        body_hash: read_named_string(row, "body_hash", "body_hash")?,
        visibility: read_named_optional_string(row, "visibility", "visibility")?,
        export: cartograph_domain::SymbolExportFlags::new(
            read_legacy_bool(row, "is_exported", "is_exported")?,
            read_legacy_bool(row, "is_default_export", "is_default_export")?,
        ),
        execution: cartograph_domain::SymbolExecutionFlags {
            async_symbol: read_legacy_bool(row, "is_async", "is_async")?,
            static_member: read_legacy_bool(row, "is_static", "is_static")?,
        },
    })
}

fn normalize_legacy_start_column(value: i32, kind: &str) -> Result<u32, V1PostgresImportError> {
    match value {
        -1 if kind == "module" => Ok(0),
        value => u32::try_from(value).map_err(|_| invalid_source("start_column")),
    }
}

async fn load_source_edges(
    connection: &mut sqlx_postgres::PgConnection,
    state: &mut SourceReadState<'_, '_>,
) -> Result<Vec<SourceEdge>, V1PostgresImportError> {
    let sql = format!(
        r#"SELECT source, target, kind, confidence, line, col, metadata
            FROM {}."edges"
            WHERE kind <> 'similar_to' ORDER BY id LIMIT $1"#,
        state.plan.quoted_schema
    );
    let mut rows = query(AssertSqlSafe(sql))
        .bind(state.plan.limit)
        .fetch(&mut *connection);
    let mut edges = Vec::new();
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|_| database_error("read-source-edges"))?
    {
        let raw_metadata = read_named_optional_string(&row, "metadata", "edge_metadata")?;
        let edge = SourceEdge {
            source: read_named_string(&row, "source", "edge_source")?,
            target: read_named_string(&row, "target", "edge_target")?,
            kind: read_named_string(&row, "kind", "edge_kind")?,
            confidence: read_named_optional_string(&row, "confidence", "edge_confidence")?,
            line: read_named_optional_u32(&row, "line", "edge_line")?,
            column: read_named_optional_u32(&row, "col", "edge_column")?,
            metadata: parse_legacy_edge_metadata(raw_metadata.as_deref())?,
        };
        hash_edge(state.fingerprint, &edge, raw_metadata.as_deref());
        edges.push(edge);
    }
    require_source_count(edges.len(), state.plan.expected.counts.edges, "edge_counts")?;
    Ok(edges)
}

async fn load_source_references(
    connection: &mut sqlx_postgres::PgConnection,
    state: &mut SourceReadState<'_, '_>,
) -> Result<Vec<SourceReference>, V1PostgresImportError> {
    let sql = format!(
        r#"SELECT from_node_id, reference_name, reference_kind, line, col, file_path,
                  language, candidates, site_count, extra_lines
            FROM {}."unresolved_refs" ORDER BY id LIMIT $1"#,
        state.plan.quoted_schema
    );
    let mut rows = query(AssertSqlSafe(sql))
        .bind(state.plan.limit)
        .fetch(&mut *connection);
    let mut references = Vec::new();
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|_| database_error("read-source-references"))?
    {
        let decoded = decode_source_reference(&row)?;
        hash_reference(
            state.fingerprint,
            ReferenceHashEvidence {
                reference: &decoded.reference,
                raw_candidates: decoded.raw_candidates.as_deref(),
                raw_extra_lines: decoded.raw_extra_lines.as_deref(),
            },
        );
        references.push(decoded.reference);
    }
    require_source_count(
        references.len(),
        state.plan.expected.counts.references,
        "reference_counts",
    )?;
    Ok(references)
}

fn decode_source_reference(
    row: &sqlx_postgres::PgRow,
) -> Result<DecodedSourceReference, V1PostgresImportError> {
    let file_path = read_named_string(row, "file_path", "reference_file_path")?;
    let raw_candidates = read_named_optional_string(row, "candidates", "reference_candidates")?;
    let raw_extra_lines = read_named_optional_string(row, "extra_lines", "reference_extra_lines")?;
    let site_count = read_named_u32(row, "site_count", "reference_site_count")?;
    let mut extra_lines =
        parse_legacy_u32_array(raw_extra_lines.as_deref(), "reference_extra_lines")?;
    if extra_lines.len() >= usize::try_from(site_count).unwrap_or(usize::MAX) {
        extra_lines.clear();
    }
    Ok(DecodedSourceReference {
        reference: SourceReference {
            from_node_id: read_named_string(row, "from_node_id", "reference_owner")?,
            reference_name: read_named_string(row, "reference_name", "reference_name")?,
            reference_kind: read_named_string(row, "reference_kind", "reference_kind")?,
            line: read_named_u32(row, "line", "reference_line")?,
            column: read_named_u32(row, "col", "reference_column")?,
            file_path: NormalizedPath::parse(&file_path)
                .map_err(|_| invalid_source("reference_file_path"))?
                .into_string(),
            language: read_supported_language(row, "language", "reference_language")?,
            candidates: parse_legacy_string_array(
                raw_candidates.as_deref(),
                "reference_candidates",
            )?,
            site_count,
            extra_lines,
        },
        raw_candidates,
        raw_extra_lines,
    })
}

struct ImportRunInitialization<'a> {
    request: &'a V1PostgresImportRequest,
    report: &'a V1PostgresDryRunReport,
    project_id: &'a ProjectId,
    destination: String,
}

struct CheckpointInsert<'a> {
    destination: &'a str,
    import_id: &'a GenerationId,
    checkpoint: V1PostgresImportCheckpoint,
}

struct CheckpointAdvance<'a> {
    import_id: &'a GenerationId,
    previous: V1PostgresImportCheckpoint,
    next: V1PostgresImportCheckpoint,
}

async fn initialize_import_run(
    database: &CartographDatabase,
    initialization: ImportRunInitialization<'_>,
) -> Result<ImportRun, V1PostgresImportError> {
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("initialize-begin"))?;
    crate::database::set_local_statement_timeout(
        &mut transaction,
        initialization.request.execution.statement_timeout,
    )
    .await
    .map_err(|()| database_error("initialize-statement-timeout"))?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "{IMPORT_LOCK_NAMESPACE}:{}:{}:{}",
            database.schema.as_str(),
            initialization.project_id,
            initialization.request.source.source_schema.as_str()
        ))
        .execute(&mut *transaction)
        .await
        .map_err(|_| database_error("initialize-lock"))?;
    let run = match load_import_run(&mut transaction, &initialization).await? {
        Some(run) => run,
        None => create_import_run(&mut transaction, &initialization).await?,
    };
    transaction
        .commit()
        .await
        .map_err(|_| database_error("initialize-commit"))?;
    Ok(run)
}

async fn load_import_run(
    connection: &mut sqlx_postgres::PgConnection,
    initialization: &ImportRunInitialization<'_>,
) -> Result<Option<ImportRun>, V1PostgresImportError> {
    let load_sql = format!(
        r#"SELECT runs.generation_id::text AS generation_id,
                  runs.source_fingerprint, runs.content_digest,
                  source_files, source_symbols, source_edges, source_edge_sites,
                  source_references, source_reference_sites, source_documents, checkpoint,
                  generations.state AS generation_state
            FROM {}."v1_import_runs" AS runs
            LEFT JOIN {}."index_generations" AS generations
              ON generations.project_id = runs.project_id
             AND generations.generation_id = runs.generation_id
            WHERE runs.project_id = CAST($1 AS uuid) AND runs.source_schema = $2
            FOR UPDATE OF runs"#,
        initialization.destination, initialization.destination
    );
    let Some(row) = query(AssertSqlSafe(load_sql))
        .bind(initialization.project_id.as_str())
        .bind(initialization.request.source.source_schema.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|_| database_error("load-run"))?
    else {
        return Ok(None);
    };
    let mut run = decode_import_run(&row, initialization.report)?;
    run.checkpoint_history =
        load_checkpoint_history(connection, &initialization.destination, &run.generation_id)
            .await?;
    validate_checkpoint_history(&run)?;
    let state = read_named_optional_string(&row, "generation_state", "generation_state")?
        .map(|raw| parse_generation_state(&raw))
        .transpose()?;
    if retryable_stale_import(&run, state) {
        let delete_sql = format!(
            r#"DELETE FROM {}."v1_import_runs" WHERE import_id = CAST($1 AS uuid)"#,
            initialization.destination
        );
        let deleted = query(AssertSqlSafe(delete_sql))
            .bind(run.generation_id.as_str())
            .execute(connection)
            .await
            .map_err(|_| database_error("reset-failed-run"))?;
        if deleted.rows_affected() != EXPECTED_MUTATED_ROWS {
            return Err(database_error("reset-failed-run"));
        }
        return Ok(None);
    }
    validate_checkpoint_state(&run, state)?;
    Ok(Some(run))
}

fn retryable_stale_import(run: &ImportRun, state: Option<GenerationState>) -> bool {
    state == Some(GenerationState::Failed)
        && run.checkpoint == V1PostgresImportCheckpoint::Bm25Rebuilt
}

async fn load_checkpoint_history(
    connection: &mut sqlx_postgres::PgConnection,
    destination: &str,
    import_id: &GenerationId,
) -> Result<Vec<V1PostgresImportCheckpoint>, V1PostgresImportError> {
    let sql = format!(
        r#"SELECT checkpoint FROM {destination}."v1_import_checkpoints"
            WHERE import_id = CAST($1 AS uuid)
            ORDER BY checkpoint_id"#
    );
    query(AssertSqlSafe(sql))
        .bind(import_id.as_str())
        .fetch_all(connection)
        .await
        .map_err(|_| database_error("load-checkpoint-history"))?
        .iter()
        .map(|row| {
            V1PostgresImportCheckpoint::parse(&read_named_string(row, "checkpoint", "checkpoint")?)
        })
        .collect()
}

fn decode_import_run(
    row: &sqlx_postgres::PgRow,
    report: &V1PostgresDryRunReport,
) -> Result<ImportRun, V1PostgresImportError> {
    let generation_id =
        GenerationId::parse(&read_named_string(row, "generation_id", "generation_id")?)
            .map_err(|_| V1PostgresImportError::CorruptCheckpoint)?;
    let recorded_counts = V1PostgresImportCounts {
        files: read_named_u64(row, "source_files", "source_files")?,
        symbols: read_named_u64(row, "source_symbols", "source_symbols")?,
        edges: read_named_u64(row, "source_edges", "source_edges")?,
        edge_sites: read_named_u64(row, "source_edge_sites", "source_edge_sites")?,
        references: read_named_u64(row, "source_references", "source_references")?,
        reference_sites: read_named_u64(row, "source_reference_sites", "source_reference_sites")?,
        documents: read_named_u64(row, "source_documents", "source_documents")?,
    };
    let fingerprint = read_named_string(row, "source_fingerprint", "source_fingerprint")?;
    let content_digest = read_named_string(row, "content_digest", "content_digest")?;
    let checkpoint =
        V1PostgresImportCheckpoint::parse(&read_named_string(row, "checkpoint", "checkpoint")?)?;
    if fingerprint != report.source_fingerprint.as_str()
        || content_digest != report.content_digest.as_str()
        || recorded_counts != report.counts
    {
        return Err(V1PostgresImportError::SourceFingerprintChanged);
    }
    Ok(ImportRun {
        generation_id,
        checkpoint,
        checkpoint_history: Vec::new(),
        resumed: true,
    })
}

async fn create_import_run(
    connection: &mut sqlx_postgres::PgConnection,
    initialization: &ImportRunInitialization<'_>,
) -> Result<ImportRun, V1PostgresImportError> {
    let reserve_sql = format!(
        r#"UPDATE {}."projects"
            SET next_generation_sequence = next_generation_sequence + 1,
                updated_at = clock_timestamp()
            WHERE project_id = CAST($1 AS uuid)
            RETURNING next_generation_sequence - 1 AS generation_sequence"#,
        initialization.destination
    );
    let sequence_row = query(AssertSqlSafe(reserve_sql))
        .bind(initialization.project_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|_| database_error("reserve-generation-sequence"))?
        .ok_or_else(|| database_error("reserve-generation-sequence"))?;
    let sequence = read_named_i64(&sequence_row, "generation_sequence", "generation_sequence")?;
    if sequence < 1 {
        return Err(database_error("reserve-generation-sequence"));
    }
    let generation_id = deterministic_generation_id(initialization, sequence);
    let insert_generation_sql = format!(
        r#"INSERT INTO {}."index_generations" (
                project_id, generation_id, generation_sequence, source_revision, worker_count
            ) VALUES (CAST($1 AS uuid), CAST($2 AS uuid), $3, $4, $5)"#,
        initialization.destination
    );
    let inserted = query(AssertSqlSafe(insert_generation_sql))
        .bind(initialization.project_id.as_str())
        .bind(generation_id.as_str())
        .bind(sequence)
        .bind(initialization.report.source_revision.as_str())
        .bind(IMPORT_WORKER_COUNT)
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("create-generation"))?;
    if inserted.rows_affected() != EXPECTED_MUTATED_ROWS {
        return Err(database_error("create-generation"));
    }
    let insert_run_sql = format!(
        r#"INSERT INTO {}."v1_import_runs" (
                import_id, project_id, generation_id, source_schema,
                source_fingerprint, content_digest, source_files, source_symbols,
                source_edges, source_edge_sites, source_references,
                source_reference_sites, source_documents, checkpoint
            ) VALUES (
                CAST($1 AS uuid), CAST($2 AS uuid), CAST($1 AS uuid), $3,
                $4, $5, $6, $7, $8, $9, $10, $11, $12, 'staged'
            )"#,
        initialization.destination
    );
    query(AssertSqlSafe(insert_run_sql))
        .bind(generation_id.as_str())
        .bind(initialization.project_id.as_str())
        .bind(initialization.request.source.source_schema.as_str())
        .bind(initialization.report.source_fingerprint.as_str())
        .bind(initialization.report.content_digest.as_str())
        .bind(u64_to_i64(initialization.report.counts.files)?)
        .bind(u64_to_i64(initialization.report.counts.symbols)?)
        .bind(u64_to_i64(initialization.report.counts.edges)?)
        .bind(u64_to_i64(initialization.report.counts.edge_sites)?)
        .bind(u64_to_i64(initialization.report.counts.references)?)
        .bind(u64_to_i64(initialization.report.counts.reference_sites)?)
        .bind(u64_to_i64(initialization.report.counts.documents)?)
        .execute(&mut *connection)
        .await
        .map_err(|_| database_error("record-run"))?;
    insert_checkpoint(
        connection,
        CheckpointInsert {
            destination: &initialization.destination,
            import_id: &generation_id,
            checkpoint: V1PostgresImportCheckpoint::Staged,
        },
    )
    .await?;
    Ok(ImportRun {
        generation_id,
        checkpoint: V1PostgresImportCheckpoint::Staged,
        checkpoint_history: vec![V1PostgresImportCheckpoint::Staged],
        resumed: false,
    })
}

fn deterministic_generation_id(
    initialization: &ImportRunInitialization<'_>,
    sequence: i64,
) -> GenerationId {
    let mut hasher = blake3::Hasher::new();
    let sequence_bytes = sequence.to_be_bytes();
    for field in [
        initialization.project_id.as_str().as_bytes(),
        initialization.report.source_revision.as_str().as_bytes(),
        initialization.report.source_fingerprint.as_str().as_bytes(),
        sequence_bytes.as_slice(),
    ] {
        hash_field(&mut hasher, field);
    }
    GenerationId::from_uuid_v8(deterministic_uuid_bytes(
        b"generation",
        hasher.finalize().as_bytes(),
    ))
}

async fn append_checkpoint(
    database: &CartographDatabase,
    advance: CheckpointAdvance<'_>,
    statement_timeout: Duration,
) -> Result<(), V1PostgresImportError> {
    if advance.next.rank() < advance.previous.rank()
        || advance.next == V1PostgresImportCheckpoint::Validated
    {
        return Err(V1PostgresImportError::CorruptCheckpoint);
    }
    let destination = crate::database::quoted_schema(&database.schema);
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("checkpoint-begin"))?;
    crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
        .await
        .map_err(|()| database_error("checkpoint-statement-timeout"))?;
    let update_sql = format!(
        r#"UPDATE {destination}."v1_import_runs"
            SET checkpoint = $2, updated_at = clock_timestamp()
            WHERE import_id = CAST($1 AS uuid) AND checkpoint = $3"#
    );
    let updated = query(AssertSqlSafe(update_sql))
        .bind(advance.import_id.as_str())
        .bind(advance.next.as_str())
        .bind(advance.previous.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|_| database_error("update-checkpoint"))?;
    if updated.rows_affected() != 1 {
        let _ = transaction.rollback().await;
        return Err(V1PostgresImportError::CorruptCheckpoint);
    }
    insert_checkpoint(
        &mut transaction,
        CheckpointInsert {
            destination: &destination,
            import_id: advance.import_id,
            checkpoint: advance.next,
        },
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| database_error("checkpoint-commit"))
}

async fn insert_checkpoint(
    connection: &mut sqlx_postgres::PgConnection,
    insertion: CheckpointInsert<'_>,
) -> Result<(), V1PostgresImportError> {
    let sql = format!(
        r#"INSERT INTO {}."v1_import_checkpoints" (import_id, checkpoint)
            VALUES (CAST($1 AS uuid), $2)
            ON CONFLICT (import_id, checkpoint) DO NOTHING"#,
        insertion.destination
    );
    query(AssertSqlSafe(sql))
        .bind(insertion.import_id.as_str())
        .bind(insertion.checkpoint.as_str())
        .execute(connection)
        .await
        .map(|_| ())
        .map_err(|_| database_error("append-checkpoint"))
}

async fn load_generation_state(
    database: &CartographDatabase,
    locator: GenerationLocator<'_>,
    statement_timeout: Duration,
) -> Result<Option<GenerationState>, V1PostgresImportError> {
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("load-generation-state-begin"))?;
    crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
        .await
        .map_err(|()| database_error("load-generation-state-timeout"))?;
    let destination = crate::database::quoted_schema(&database.schema);
    let sql = format!(
        r#"SELECT state AS generation_state FROM {destination}."index_generations"
            WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)"#
    );
    let state = query(AssertSqlSafe(sql))
        .bind(locator.project_id.as_str())
        .bind(locator.generation_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| database_error("load-generation-state"))?
        .map(|row| {
            parse_generation_state(&read_named_string(
                &row,
                "generation_state",
                "generation_state",
            )?)
        })
        .transpose()?;
    transaction
        .commit()
        .await
        .map_err(|_| database_error("load-generation-state-commit"))?;
    Ok(state)
}

async fn verify_destination(
    database: &CartographDatabase,
    verification: DestinationVerification<'_>,
    statement_timeout: Duration,
) -> Result<(), V1PostgresImportError> {
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("verify-destination-begin"))?;
    query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *transaction)
        .await
        .map_err(|_| database_error("verify-destination-isolation"))?;
    crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
        .await
        .map_err(|()| database_error("verify-destination-timeout"))?;
    let destination = crate::database::quoted_schema(&database.schema);
    let sql = format!(
        r#"SELECT generations.content_digest, generations.content_digest_version,
            (SELECT count(*)::bigint FROM {destination}."files"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS file_count,
            (SELECT count(*)::bigint FROM {destination}."symbols"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS symbol_count,
            (SELECT count(*)::bigint FROM {destination}."edges"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS edge_count,
            (SELECT COALESCE(sum(site_count), 0)::bigint FROM {destination}."edges"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS edge_site_count,
            (SELECT count(*)::bigint FROM {destination}."references"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS reference_count,
            (SELECT COALESCE(sum(site_count), 0)::bigint FROM {destination}."references"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS reference_site_count,
            (SELECT count(*)::bigint FROM {destination}."search_documents"
                WHERE project_id = CAST($1 AS uuid) AND generation_id = CAST($2 AS uuid)) AS document_count
            FROM {destination}."index_generations" AS generations
            WHERE generations.project_id = CAST($1 AS uuid)
              AND generations.generation_id = CAST($2 AS uuid)"#
    );
    let row = query(AssertSqlSafe(sql))
        .bind(verification.locator.project_id.as_str())
        .bind(verification.locator.generation_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| database_error("verify-counts"))?
        .ok_or(V1PostgresImportError::CorruptCheckpoint)?;
    let content_digest =
        read_named_optional_string(&row, "content_digest", "destination_content_digest")?
            .ok_or(V1PostgresImportError::CorruptCheckpoint)?;
    let digest_version = row
        .try_get::<Option<i16>, _>("content_digest_version")
        .map_err(|_| V1PostgresImportError::CorruptCheckpoint)?
        .ok_or(V1PostgresImportError::CorruptCheckpoint)?;
    if content_digest != verification.analysis.report.content_digest.as_str()
        || digest_version != GenerationDigestVersion::V6.database_value()
    {
        return Err(V1PostgresImportError::CorruptCheckpoint);
    }
    let counts = V1PostgresImportCounts {
        files: read_named_u64(&row, "file_count", "destination_files")?,
        symbols: read_named_u64(&row, "symbol_count", "destination_symbols")?,
        edges: read_named_u64(&row, "edge_count", "destination_edges")?,
        edge_sites: read_named_u64(&row, "edge_site_count", "destination_edge_sites")?,
        references: read_named_u64(&row, "reference_count", "destination_references")?,
        reference_sites: read_named_u64(
            &row,
            "reference_site_count",
            "destination_reference_sites",
        )?,
        documents: read_named_u64(&row, "document_count", "destination_documents")?,
    };
    if counts != verification.analysis.report.counts {
        return Err(invalid_source("destination_counts"));
    }
    transaction
        .commit()
        .await
        .map_err(|_| database_error("verify-destination-commit"))?;
    Ok(())
}

async fn rebuild_bm25(
    database: &CartographDatabase,
    rebuild: Bm25Rebuild<'_>,
) -> Result<(), V1PostgresImportError> {
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error("bm25-begin"))?;
    if crate::database::set_local_statement_timeout(&mut transaction, rebuild.statement_timeout)
        .await
        .is_err()
    {
        let _ = transaction.rollback().await;
        return Err(V1PostgresImportError::InvalidInput {
            field: "statement_timeout",
        });
    }
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "{BM25_REBUILD_LOCK_NAMESPACE}:{}",
            database.schema.as_str()
        ))
        .execute(&mut *transaction)
        .await
        .map_err(|_| database_error("bm25-lock"))?;
    let destination = crate::database::quoted_schema(&database.schema);
    require_live_bm25_fence(&mut transaction, &destination, rebuild.fence).await?;
    let generation_id = rebuild
        .fence
        .target()
        .generation_id()
        .ok_or(V1PostgresImportError::LeaseFenceLost)?;
    crate::search_relation::rebuild_generation_search_relation(
        &mut transaction,
        crate::search_relation::GenerationSearchBuild {
            schema: &database.schema,
            project_id: rebuild.fence.target().project_id(),
            generation_id,
            content_digest: rebuild.content_digest,
        },
    )
    .await
    .map_err(|_| database_error("bm25-generation-build"))?;
    // The row lock prevents takeover while rebuilding, but expiry still advances.
    require_live_bm25_fence(&mut transaction, &destination, rebuild.fence).await?;
    transaction
        .commit()
        .await
        .map_err(|_| database_error("bm25-commit"))
}

async fn require_live_bm25_fence(
    connection: &mut sqlx_postgres::PgConnection,
    destination: &str,
    fence: &crate::LeaseFence,
) -> Result<(), V1PostgresImportError> {
    let generation_id = fence
        .target()
        .generation_id()
        .ok_or(V1PostgresImportError::LeaseFenceLost)?;
    let sql = format!(
        r#"SELECT 1 FROM {destination}."project_operation_leases"
            WHERE project_id = CAST($1 AS uuid)
              AND operation = 'migration'
              AND lease_id = CAST($2 AS uuid)
              AND generation_id = CAST($3 AS uuid)
              AND expires_at > clock_timestamp()
            FOR UPDATE"#
    );
    if query(AssertSqlSafe(sql))
        .bind(fence.target().project_id().as_str())
        .bind(fence.lease_id().as_str())
        .bind(generation_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("bm25-lease"))?
        .is_some()
    {
        Ok(())
    } else {
        Err(V1PostgresImportError::LeaseFenceLost)
    }
}

async fn release_and_error<T>(
    database: &CartographDatabase,
    failure: LeaseReleaseFailure<'_>,
) -> Result<T, V1PostgresImportError> {
    let release = database
        .release_lease_bounded(failure.lease, failure.statement_timeout)
        .await;
    if failure.error == V1PostgresImportError::LeaseFenceLost {
        // An expired or replaced token is expected to fail exact live-token release.
        // Preserve the primary fencing result; the stale row is harmless and takeover-safe.
        return Err(failure.error);
    }
    release.map_err(|_| database_error("release-migration-lease"))?;
    Err(failure.error)
}

fn import_report(input: ImportReportInput<'_>) -> V1PostgresImportReport {
    V1PostgresImportReport {
        project_id: input.project_id,
        generation_id: input.generation_id,
        source_fingerprint: input.report.source_fingerprint.clone(),
        content_digest: input.report.content_digest.clone(),
        counts: input.report.counts,
        resumed: input.resumed,
        checkpoint: V1PostgresImportCheckpoint::Complete,
    }
}

fn canonical_counts(
    facts: &CanonicalGenerationFacts,
) -> Result<V1PostgresImportCounts, V1PostgresImportError> {
    Ok(V1PostgresImportCounts {
        files: usize_to_u64(facts.files().len())?,
        symbols: usize_to_u64(facts.symbols().len())?,
        edges: usize_to_u64(facts.edges().len())?,
        edge_sites: facts
            .edges()
            .iter()
            .try_fold(0_u64, |total, edge| {
                total.checked_add(u64::from(edge.site_count))
            })
            .ok_or(V1PostgresImportError::SourceLimit)?,
        references: usize_to_u64(facts.references().len())?,
        reference_sites: facts
            .references()
            .iter()
            .try_fold(0_u64, |total, reference| {
                total.checked_add(u64::from(reference.site_count))
            })
            .ok_or(V1PostgresImportError::SourceLimit)?,
        documents: usize_to_u64(facts.documents().len())?,
    })
}

fn deterministic_uuid_bytes(domain: &[u8], legacy_id: &[u8]) -> [u8; UUID_BYTE_LENGTH] {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, b"cartograph-v1.1.33-uuid-v8");
    hash_field(&mut hasher, domain);
    hash_field(&mut hasher, legacy_id);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; UUID_BYTE_LENGTH];
    bytes.copy_from_slice(&digest.as_bytes()[..UUID_BYTE_LENGTH]);
    bytes[UUID_VERSION_BYTE] =
        (bytes[UUID_VERSION_BYTE] & UUID_VERSION_CLEAR_MASK) | UUID_VERSION_EIGHT;
    bytes[UUID_VARIANT_BYTE] =
        (bytes[UUID_VARIANT_BYTE] & UUID_VARIANT_CLEAR_MASK) | UUID_VARIANT_RFC_4122;
    bytes
}

fn hash_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
    hasher.update(&length.to_be_bytes());
    hasher.update(value);
}

fn hash_optional_field(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hash_field(hasher, &[1]);
            hash_field(hasher, value.as_bytes());
        }
        None => hash_field(hasher, &[0]),
    }
}

fn hash_optional_u32(hasher: &mut blake3::Hasher, value: Option<u32>) {
    match value {
        Some(value) => {
            hash_field(hasher, &[1]);
            hash_field(hasher, &value.to_be_bytes());
        }
        None => hash_field(hasher, &[0]),
    }
}

fn hash_node(hasher: &mut blake3::Hasher, node: &SourceNode) {
    for value in [
        node.legacy_id.as_bytes(),
        node.name.as_bytes(),
        node.kind.as_bytes(),
        node.qualified_name.as_bytes(),
        node.file_path.as_bytes(),
        node.language.as_bytes(),
        node.body_hash.as_bytes(),
    ] {
        hash_field(hasher, value);
    }
    hash_optional_field(hasher, node.docstring.as_deref());
    hash_optional_field(hasher, node.signature.as_deref());
    hash_optional_field(hasher, node.visibility.as_deref());
    for value in [
        node.export.exported,
        node.export.default_export,
        node.execution.async_symbol,
        node.execution.static_member,
    ] {
        hash_field(hasher, &[u8::from(value)]);
    }
    for value in [
        node.start_line,
        node.end_line,
        node.start_column,
        node.end_column,
    ] {
        hash_field(hasher, &value.to_be_bytes());
    }
}

fn hash_edge(hasher: &mut blake3::Hasher, edge: &SourceEdge, raw_metadata: Option<&str>) {
    for value in [
        edge.source.as_bytes(),
        edge.target.as_bytes(),
        edge.kind.as_bytes(),
    ] {
        hash_field(hasher, value);
    }
    hash_optional_field(hasher, edge.confidence.as_deref());
    hash_optional_u32(hasher, edge.line);
    hash_optional_u32(hasher, edge.column);
    hash_optional_field(hasher, raw_metadata);
}

fn hash_reference(hasher: &mut blake3::Hasher, evidence: ReferenceHashEvidence<'_>) {
    for value in [
        evidence.reference.from_node_id.as_bytes(),
        evidence.reference.reference_name.as_bytes(),
        evidence.reference.reference_kind.as_bytes(),
        evidence.reference.file_path.as_bytes(),
        evidence.reference.language.as_bytes(),
    ] {
        hash_field(hasher, value);
    }
    hash_field(hasher, &evidence.reference.line.to_be_bytes());
    hash_field(hasher, &evidence.reference.column.to_be_bytes());
    hash_field(hasher, &evidence.reference.site_count.to_be_bytes());
    hash_optional_field(hasher, evidence.raw_candidates);
    hash_optional_field(hasher, evidence.raw_extra_lines);
}

fn parse_generation_state(raw: &str) -> Result<GenerationState, V1PostgresImportError> {
    match raw {
        "staging" => Ok(GenerationState::Staging),
        "ready" => Ok(GenerationState::Ready),
        "current" => Ok(GenerationState::Current),
        "superseded" => Ok(GenerationState::Superseded),
        "failed" => Ok(GenerationState::Failed),
        "retiring" => Ok(GenerationState::Retiring),
        _ => Err(V1PostgresImportError::CorruptCheckpoint),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    encode_hex(Sha256::digest(bytes).as_slice())
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(HEX_CHARACTERS_PER_BYTE));
    for &byte in bytes {
        encoded.push(char::from(HEX_DIGITS[usize::from(byte >> HEX_NIBBLE_BITS)]));
        encoded.push(char::from(HEX_DIGITS[usize::from(byte & HEX_NIBBLE_MASK)]));
    }
    encoded
}

fn read_supported_language(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<String, V1PostgresImportError> {
    let language = read_named_string(row, column, field)?;
    SourceLanguage::from_stable_str(&language)
        .filter(|parsed| parsed.is_native_indexable())
        .map(|_| language)
        .ok_or_else(|| invalid_source(field))
}

fn read_named_string(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<String, V1PostgresImportError> {
    row.try_get::<String, _>(column)
        .map_err(|_| invalid_source(field))
}

fn read_named_optional_string(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<Option<String>, V1PostgresImportError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|_| invalid_source(field))
}

fn read_named_i64(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<i64, V1PostgresImportError> {
    row.try_get::<i64, _>(column)
        .map_err(|_| invalid_source(field))
}

fn read_named_optional_u32(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<Option<u32>, V1PostgresImportError> {
    row.try_get::<Option<i32>, _>(column)
        .map_err(|_| invalid_source(field))?
        .map(|value| u32::try_from(value).map_err(|_| invalid_source(field)))
        .transpose()
}

fn read_legacy_bool(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<bool, V1PostgresImportError> {
    match row
        .try_get::<Option<i32>, _>(column)
        .map_err(|_| invalid_source(field))?
    {
        None | Some(0) => Ok(false),
        Some(1) => Ok(true),
        Some(_) => Err(invalid_source(field)),
    }
}

fn read_named_bool(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<bool, V1PostgresImportError> {
    row.try_get::<bool, _>(column)
        .map_err(|_| invalid_source(field))
}

fn read_named_u64(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<u64, V1PostgresImportError> {
    let value = read_named_i64(row, column, field)?;
    u64::try_from(value).map_err(|_| invalid_source(field))
}

fn read_named_u32(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
    field: &'static str,
) -> Result<u32, V1PostgresImportError> {
    let value = row
        .try_get::<i32, _>(column)
        .map_err(|_| invalid_source(field))?;
    u32::try_from(value).map_err(|_| invalid_source(field))
}

fn usize_to_u64(value: usize) -> Result<u64, V1PostgresImportError> {
    u64::try_from(value).map_err(|_| V1PostgresImportError::SourceLimit)
}

fn u64_to_i64(value: u64) -> Result<i64, V1PostgresImportError> {
    i64::try_from(value).map_err(|_| V1PostgresImportError::SourceLimit)
}

const fn invalid_source(field: &'static str) -> V1PostgresImportError {
    V1PostgresImportError::InvalidSourceData { field }
}

const fn database_error(operation: &'static str) -> V1PostgresImportError {
    V1PostgresImportError::DatabaseOperation { operation }
}

fn storage_mutation_error(
    error: &crate::StorageError,
    operation: &'static str,
) -> V1PostgresImportError {
    if error == &crate::StorageError::LeaseFenceLost {
        V1PostgresImportError::LeaseFenceLost
    } else {
        database_error(operation)
    }
}

fn lease_heartbeat_error(error: &crate::LeaseError) -> V1PostgresImportError {
    if error == &crate::LeaseError::Lost {
        V1PostgresImportError::LeaseFenceLost
    } else {
        database_error("heartbeat-migration-lease")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DERIVED_ROW_ALLOCATION_ALLOWANCE, ImportRun, PreflightMemoryInput,
        SOURCE_JSON_EXPANSION_FACTOR, SOURCE_LINE_OFFSET_BYTES, SOURCE_PATH_TRANSIENT_BYTES,
        SOURCE_ROW_ALLOCATION_ALLOWANCE, SOURCE_ROW_DECODE_TRANSIENT_BYTES, SourceFile,
        UUID_VARIANT_BYTE, UUID_VARIANT_CLEAR_MASK, UUID_VARIANT_RFC_4122, UUID_VERSION_BYTE,
        UUID_VERSION_CLEAR_MASK, UUID_VERSION_EIGHT, V1PostgresImportCheckpoint,
        V1PostgresImportCounts, V1PostgresImportError, deterministic_uuid_bytes,
        lease_heartbeat_error, line_starts, native_import_languages, normalize_legacy_start_column,
        preflight_memory_admission, require_source_manifest, retryable_stale_import, sha256_hex,
        source_matches_native_language, storage_mutation_error, v1_content_hash_matches,
        validate_checkpoint_state,
    };
    use cartograph_domain::{
        ContentDigest, GenerationId, GenerationState, NormalizedPath, SourceLanguage,
        SourceManifestDigestBuilder,
    };
    use std::collections::{BTreeMap, BTreeSet};

    const UTF8_LINE_STARTS: [u32; 3] = [0, 2, 5];
    const PREFLIGHT_METADATA_BYTES: u64 = 20;
    const PREFLIGHT_JSON_BYTES: u64 = 5;
    const PREFLIGHT_SOURCE_POLICY_BYTES: u64 = 32;
    const PREFLIGHT_WORKING_SOURCE_POLICY_BYTES: u64 = 100;
    const PREFLIGHT_GENEROUS_WORKING_BYTES: u64 = 2_000_000;
    const PREFLIGHT_FIRST_REJECTED_SOURCE_BYTES: u64 = 13;
    const PREFLIGHT_CHECKOUT_BYTES: u64 = 5;
    const PREFLIGHT_EXPECTED_FILE_COUNT: u64 = 1;
    const PREFLIGHT_MINIMUM_DERIVED_ROWS: u64 = 2;
    const PREFLIGHT_SOURCE_POLICY_REMAINDER: u64 =
        PREFLIGHT_SOURCE_POLICY_BYTES - PREFLIGHT_METADATA_BYTES;
    const TAGS_IMPORT_FIXTURES: [(&str, &str, &str); 6] = [
        (
            "tags/elixir.ex",
            "defmodule T2Elixir do\n  def tagscanary, do: :ok\nend\n",
            "elixir",
        ),
        ("tags/haskell.hs", "tagscanary x = x\n", "haskell"),
        (
            "tags/julia.jl",
            "function tagscanary(x)\n  x\nend\n",
            "julia",
        ),
        ("tags/ocaml.ml", "let tagscanary x = x\n", "ocaml"),
        (
            "tags/ocaml.mli",
            "val tagscanary : int -> int\n",
            "ocaml_interface",
        ),
        (
            "tags/verilog.sv",
            "module T2Verilog;\nfunction int tagscanary; endfunction\nendmodule\n",
            "verilog",
        ),
    ];

    #[test]
    fn legacy_identity_mapping_is_deterministic_domain_separated_uuid_v8() {
        let first = deterministic_uuid_bytes(b"symbol", b"legacy-node");
        let repeated = deterministic_uuid_bytes(b"symbol", b"legacy-node");
        let other_kind = deterministic_uuid_bytes(b"file", b"legacy-node");
        let other_id = deterministic_uuid_bytes(b"symbol", b"other-node");

        assert_eq!(first, repeated);
        assert_ne!(first, other_kind);
        assert_ne!(first, other_id);
        assert_eq!(
            first[UUID_VERSION_BYTE] & !UUID_VERSION_CLEAR_MASK,
            UUID_VERSION_EIGHT
        );
        assert_eq!(
            first[UUID_VARIANT_BYTE] & !UUID_VARIANT_CLEAR_MASK,
            UUID_VARIANT_RFC_4122
        );
    }

    #[test]
    fn source_hash_and_line_index_match_v1_and_utf8_byte_coordinates() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            line_starts("a\n\u{03b2}\n").as_deref(),
            Ok(UTF8_LINE_STARTS.as_slice())
        );

        let bom_source = "\u{feff}fn generated() {}\n";
        let raw_scip_hash = sha256_hex(bom_source.as_bytes());
        let stripped_extractor_hash = sha256_hex("fn generated() {}\n".as_bytes());
        assert!(v1_content_hash_matches(bom_source, &raw_scip_hash));
        assert!(v1_content_hash_matches(
            bom_source,
            &stripped_extractor_hash
        ));
        assert!(!v1_content_hash_matches(bom_source, &"0".repeat(64)));
    }

    #[test]
    fn durable_checkpoint_order_never_regresses() {
        assert!(
            V1PostgresImportCheckpoint::Staged.rank() < V1PostgresImportCheckpoint::Ready.rank()
        );
        assert!(
            V1PostgresImportCheckpoint::Bm25Rebuilt.rank()
                < V1PostgresImportCheckpoint::Complete.rank()
        );
    }

    #[test]
    fn only_the_importer_authored_stale_publication_shape_is_retryable() {
        let stale = checkpoint_run(V1PostgresImportCheckpoint::Bm25Rebuilt);
        assert!(retryable_stale_import(
            &stale,
            Some(GenerationState::Failed)
        ));

        let complete = checkpoint_run(V1PostgresImportCheckpoint::Complete);
        assert!(!retryable_stale_import(
            &complete,
            Some(GenerationState::Failed)
        ));
        assert_eq!(
            validate_checkpoint_state(&complete, Some(GenerationState::Failed)),
            Err(V1PostgresImportError::CorruptCheckpoint)
        );

        let staged = checkpoint_run(V1PostgresImportCheckpoint::Staged);
        assert!(!retryable_stale_import(&staged, None));
        assert_eq!(
            validate_checkpoint_state(&staged, None),
            Err(V1PostgresImportError::CorruptCheckpoint)
        );
    }

    fn checkpoint_run(checkpoint: V1PostgresImportCheckpoint) -> ImportRun {
        const GENERATION_ID: &str = "11111111-1111-8111-8111-111111111111";
        const LEGAL_HISTORY: [V1PostgresImportCheckpoint; 4] = [
            V1PostgresImportCheckpoint::Staged,
            V1PostgresImportCheckpoint::Ready,
            V1PostgresImportCheckpoint::Bm25Rebuilt,
            V1PostgresImportCheckpoint::Complete,
        ];
        ImportRun {
            generation_id: GenerationId::parse(GENERATION_ID)
                .unwrap_or_else(|error| panic!("test generation id is invalid: {error}")),
            checkpoint,
            checkpoint_history: LEGAL_HISTORY[..usize::from(checkpoint.rank())].to_vec(),
            resumed: true,
        }
    }

    #[test]
    fn lease_loss_is_preserved_across_generation_and_heartbeat_errors() {
        assert_eq!(
            storage_mutation_error(&crate::StorageError::LeaseFenceLost, "prepare-generation"),
            V1PostgresImportError::LeaseFenceLost
        );
        assert_eq!(
            lease_heartbeat_error(&crate::LeaseError::Lost),
            V1PostgresImportError::LeaseFenceLost
        );
        assert_eq!(
            storage_mutation_error(
                &crate::StorageError::GenerationNotFound,
                "prepare-generation"
            ),
            V1PostgresImportError::DatabaseOperation {
                operation: "prepare-generation"
            }
        );
    }

    #[test]
    fn importer_language_preflight_tracks_every_native_language() {
        let import_languages = native_import_languages();
        let unique_languages = import_languages.iter().collect::<BTreeSet<_>>();
        let native_languages = SourceLanguage::ALL
            .into_iter()
            .filter(|language| language.is_native_indexable())
            .map(|language| language.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(import_languages, native_languages);
        assert_eq!(unique_languages.len(), import_languages.len());
        assert!(import_languages.iter().all(|language| {
            SourceLanguage::from_stable_str(language)
                .is_some_and(SourceLanguage::is_native_indexable)
        }));
    }

    #[test]
    fn importer_file_admission_matches_native_discovery_and_content_classification() {
        assert!(source_matches_native_language(
            "config/app.json",
            r#"{"enabled":true}"#,
            "json",
        ));
        assert!(!source_matches_native_language(
            "notes/plain.md",
            "plain markdown without front matter",
            "liquid",
        ));
        assert!(!source_matches_native_language(
            "unrelated/widget.cmp",
            "<widget />",
            "aura",
        ));
        assert!(!source_matches_native_language(
            "config/app.json",
            r#"{"enabled":true}"#,
            "javascript",
        ));
    }

    #[test]
    fn importer_admits_every_query_backed_language_family() {
        let import_languages = native_import_languages();
        for (path, source, language) in TAGS_IMPORT_FIXTURES {
            assert!(import_languages.iter().any(|value| value == language));
            assert!(source_matches_native_language(path, source, language));
        }
    }

    #[test]
    fn importer_manifest_rejects_missing_extra_and_same_count_different_paths() {
        let complete = manifest_digest(&[("a.rs", "a"), ("b.rs", "b")]);
        let missing = manifest_files(&[("a.rs", "a")]);
        assert_eq!(
            require_source_manifest(&missing, &complete),
            Err(V1PostgresImportError::SourceManifestMismatch)
        );

        let single = manifest_digest(&[("a.rs", "a")]);
        let extra = manifest_files(&[("a.rs", "a"), ("b.rs", "b")]);
        assert_eq!(
            require_source_manifest(&extra, &single),
            Err(V1PostgresImportError::SourceManifestMismatch)
        );

        let different_path = manifest_digest(&[("b.rs", "a")]);
        assert_eq!(
            require_source_manifest(&missing, &different_path),
            Err(V1PostgresImportError::SourceManifestMismatch)
        );
        assert_eq!(
            require_source_manifest(&missing, &manifest_digest(&[("a.rs", "a")])),
            Ok(manifest_digest(&[("a.rs", "a")]))
        );
    }

    #[test]
    fn importer_normalizes_only_the_v1_synthetic_module_start_sentinel() {
        assert_eq!(normalize_legacy_start_column(-1, "module"), Ok(0));
        assert_eq!(normalize_legacy_start_column(7, "module"), Ok(7));
        assert_eq!(normalize_legacy_start_column(0, "function"), Ok(0));
        assert_eq!(
            normalize_legacy_start_column(-1, "function"),
            Err(V1PostgresImportError::InvalidSourceData {
                field: "start_column"
            })
        );
    }

    fn manifest_digest(entries: &[(&str, &str)]) -> ContentDigest {
        let mut builder = SourceManifestDigestBuilder::new(entries.len())
            .unwrap_or_else(|error| panic!("manifest builder failed: {error}"));
        for (path, source) in entries {
            let path = NormalizedPath::parse(path)
                .unwrap_or_else(|error| panic!("manifest path failed: {error}"));
            let content = ContentDigest::from_bytes(*blake3::hash(source.as_bytes()).as_bytes());
            builder
                .push(&path, &content)
                .unwrap_or_else(|error| panic!("manifest entry failed: {error}"));
        }
        builder
            .finish()
            .unwrap_or_else(|error| panic!("manifest finish failed: {error}"))
    }

    fn manifest_files(entries: &[(&str, &str)]) -> BTreeMap<String, SourceFile> {
        entries
            .iter()
            .map(|(path, source)| {
                (
                    (*path).to_owned(),
                    SourceFile {
                        language: "rust".to_owned(),
                        source: (*source).to_owned(),
                        line_starts: line_starts(source)
                            .unwrap_or_else(|error| panic!("line index failed: {error}")),
                        content_hash: ContentDigest::from_bytes(
                            *blake3::hash(source.as_bytes()).as_bytes(),
                        ),
                        has_extraction_errors: false,
                        is_test: false,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn preflight_reserves_metadata_expansion_and_actual_checkout_bytes() {
        let counts = V1PostgresImportCounts {
            files: PREFLIGHT_EXPECTED_FILE_COUNT,
            ..V1PostgresImportCounts::default()
        };
        let admission = |source_bytes, maximum_source_bytes, maximum_working_bytes| {
            preflight_memory_admission(PreflightMemoryInput {
                counts,
                source_bytes,
                source_metadata_bytes: PREFLIGHT_METADATA_BYTES,
                source_json_bytes: PREFLIGHT_JSON_BYTES,
                maximum_source_bytes,
                maximum_working_bytes,
            })
        };
        assert_eq!(
            admission(
                0,
                PREFLIGHT_SOURCE_POLICY_BYTES,
                PREFLIGHT_GENEROUS_WORKING_BYTES
            ),
            Ok(PREFLIGHT_SOURCE_POLICY_REMAINDER)
        );
        assert_eq!(
            admission(
                PREFLIGHT_FIRST_REJECTED_SOURCE_BYTES,
                PREFLIGHT_SOURCE_POLICY_BYTES,
                PREFLIGHT_GENEROUS_WORKING_BYTES,
            ),
            Err(V1PostgresImportError::SourceLimit)
        );

        let fixed_floor = PREFLIGHT_METADATA_BYTES - PREFLIGHT_JSON_BYTES
            + PREFLIGHT_JSON_BYTES * SOURCE_JSON_EXPANSION_FACTOR
            + SOURCE_ROW_ALLOCATION_ALLOWANCE
            + PREFLIGHT_MINIMUM_DERIVED_ROWS * DERIVED_ROW_ALLOCATION_ALLOWANCE
            + SOURCE_PATH_TRANSIENT_BYTES
            + SOURCE_ROW_DECODE_TRANSIENT_BYTES;
        let newline_dense_five_byte_budget = fixed_floor
            + SOURCE_LINE_OFFSET_BYTES
            + PREFLIGHT_CHECKOUT_BYTES * (1 + SOURCE_LINE_OFFSET_BYTES);
        assert_eq!(
            admission(
                PREFLIGHT_CHECKOUT_BYTES,
                PREFLIGHT_WORKING_SOURCE_POLICY_BYTES,
                newline_dense_five_byte_budget,
            ),
            Ok(PREFLIGHT_CHECKOUT_BYTES)
        );
        assert_eq!(
            admission(
                PREFLIGHT_CHECKOUT_BYTES,
                PREFLIGHT_WORKING_SOURCE_POLICY_BYTES,
                newline_dense_five_byte_budget - 1,
            ),
            Err(V1PostgresImportError::SourceLimit)
        );
    }
}
