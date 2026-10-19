use std::time::Duration;

use cartograph_domain::{FileId, GenerationId, ProjectId, SymbolId};
use serde::Serialize;
use thiserror::Error;

mod native;

const MAXIMUM_INTERCHANGE_ROWS: u64 = 5_000_000;

/// One immutable file record in a generation interchange snapshot.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterchangeFile {
    /// Stable file ID for this record.
    pub file_id: FileId,
    /// Project-relative path for this record.
    pub path: String,
    /// Language for this record.
    pub language: String,
    /// Digest-fenced content hash for this record.
    pub content_hash: String,
    /// Indexed source size in bytes.
    pub byte_size: u64,
}

/// One immutable symbol record in a generation interchange snapshot.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterchangeSymbol {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Stable file ID for this record.
    pub file_id: FileId,
    /// Symbol kind for this record.
    pub symbol_kind: String,
    /// Qualified name for this record.
    pub qualified_name: String,
    /// Signature for this record.
    pub signature: String,
    /// Code for this record.
    pub code: String,
    /// Natural text for this record.
    pub natural_text: String,
    /// Zero-based starting byte offset.
    pub start_byte: u64,
    /// Exclusive ending byte offset.
    pub end_byte: u64,
    /// One-based starting source line.
    pub start_line: u32,
    /// One-based ending source line.
    pub end_line: u32,
}

/// One typed symbol relationship in a generation interchange snapshot.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterchangeEdge {
    /// Stable source symbol ID for this record.
    pub source_symbol_id: SymbolId,
    /// Stable target symbol ID for this record.
    pub target_symbol_id: SymbolId,
    /// Edge kind for this record.
    pub edge_kind: String,
    /// Confidence for this record.
    pub confidence: f32,
    /// Provenance for this record.
    pub provenance: String,
    /// Number of site entries.
    pub site_count: u32,
}

/// One exact indexed reference site in a generation interchange snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterchangeReference {
    /// Stable file ID for this record.
    pub file_id: FileId,
    /// Optional owner symbol ID, when available.
    pub owner_symbol_id: Option<SymbolId>,
    /// Optional target symbol ID, when available.
    pub target_symbol_id: Option<SymbolId>,
    /// Reference name for this record.
    pub reference_name: String,
    /// Reference kind for this record.
    pub reference_kind: String,
    /// Zero-based starting byte offset.
    pub start_byte: u64,
    /// Exclusive ending byte offset.
    pub end_byte: u64,
    /// Number of site entries.
    pub site_count: u32,
}

/// Exactly bounded files, symbols, edges, and references from one generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterchangeSnapshot {
    /// Stable generation ID for this record.
    pub generation_id: GenerationId,
    /// Bounded files included in this result.
    pub files: Vec<InterchangeFile>,
    /// Bounded symbols included in this result.
    pub symbols: Vec<InterchangeSymbol>,
    /// Bounded edges included in this result.
    pub edges: Vec<InterchangeEdge>,
    /// Bounded references included in this result.
    pub references: Vec<InterchangeReference>,
}

/// Bounded, deadline-scoped export of one project's current generation.
pub struct InterchangeSnapshotRequest<'request> {
    /// Stable project ID for this record.
    pub project_id: &'request ProjectId,
    /// Maximum number of rows permitted by this request.
    pub maximum_rows: u64,
    /// Statement timeout for this record.
    pub statement_timeout: Duration,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
/// Errors produced while processing interchange snapshot.
pub enum InterchangeSnapshotError {
    #[error("invalid Cartograph interchange snapshot bounds")]
    /// Supplied bounds are zero, inconsistent, or exceed the hard ceiling.
    InvalidBounds,
    #[error("Cartograph has no current generation to export")]
    /// No complete current generation is available for this operation.
    CurrentGenerationUnavailable,
    #[error("Cartograph interchange snapshot exceeds its row bound")]
    /// The result exceeded its declared row ceiling.
    RowBoundExceeded,
    #[error("Cartograph interchange snapshot exceeds its native retained byte bound")]
    /// A single retained snapshot exceeds the native worker's explicit 64 MiB workspace.
    ByteBoundExceeded,
    #[error("Cartograph interchange snapshot contains corrupt stored data")]
    /// A stored row violates its durable typed contract.
    CorruptStoredValue,
    #[error("Cartograph native storage interchange snapshot failed")]
    /// native storage could not complete the named operation.
    DatabaseOperation,
}
