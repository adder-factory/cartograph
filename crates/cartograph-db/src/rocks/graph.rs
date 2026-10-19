//! Typed canonical graph records and radix-partitioned packed adjacency.

use std::{mem::size_of, sync::Arc, time::Duration};

use cartograph_domain::{ContentDigest, GenerationId};
use serde::{Deserialize, Serialize};

use crate::{CanonicalGenerationFacts, GenerationMemoryModelError};

use super::{ProjectStore, PruneReport, Result, RocksError, StorageRequest, StoredRevision};

mod codec;
mod ingest;
mod lexical;
mod names;
mod packed;
mod read;

pub use lexical::{LexicalPosting, LexicalStatistics};
pub use read::{GraphCursor, GraphLink, GraphNeighborPage, GraphSnapshot};

const MIB: usize = 1_048_576;
const FILE: &[u8] = b"cg1/f/";
const SYMBOL: &[u8] = b"cg1/s/";
const EDGE: &[u8] = b"cg1/e/";
const REFERENCE: &[u8] = b"cg1/r/";
const NUMERICAL: &[u8] = b"cg1/n/";
const DOCUMENT: &[u8] = b"cg1/d/";
const OUTGOING: &[u8] = b"cg1/o/";
const INCOMING: &[u8] = b"cg1/i/";
const MANIFEST: &[u8] = b"cg1/m";

/// Explicit base revision; stale canonical output can never silently rebase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphBase {
    /// Initialize a project with no published revision.
    Empty,
    /// Replace exactly this complete published revision.
    Current(GenerationId),
}

/// Complete source identity and exact publication base supplied by the indexer.
#[derive(Clone, Debug)]
pub struct GraphRevision {
    /// New immutable generation identity.
    pub generation_id: GenerationId,
    /// Source and admission-policy digest captured by the native pipeline owner.
    pub source_revision: ContentDigest,
    /// Expected current graph before starting this publication.
    pub base: GraphBase,
}

/// Finite canonical-input, indexing-workspace, and durable staging bounds.
#[derive(Clone, Copy, Debug)]
pub struct GraphPolicy {
    /// Maximum Rust-owned bytes in the validated canonical input.
    pub maximum_input_bytes: u64,
    /// Temporary key indexes and adjacency entries, excluding bounded I/O buffers.
    pub maximum_working_bytes: usize,
    /// Maximum encoded bytes accumulated before a durable staging batch.
    pub maximum_batch_bytes: usize,
}

impl Default for GraphPolicy {
    fn default() -> Self {
        Self {
            maximum_input_bytes: 128 * 1_048_576,
            maximum_working_bytes: 32 * MIB,
            maximum_batch_bytes: 4 * MIB,
        }
    }
}

impl GraphPolicy {
    fn validate(self) -> Result<Self> {
        if self.maximum_input_bytes == 0
            || self.maximum_input_bytes > 512 * 1_048_576
            || !(MIB..=512 * MIB).contains(&self.maximum_working_bytes)
            || !(MIB..=16 * MIB).contains(&self.maximum_batch_bytes)
        {
            return Err(RocksError::Limit {
                resource: "graph_policy",
            });
        }
        Ok(self)
    }
}

/// Canonical facts with an immutable, fully measured admission reservation.
///
/// Construction checks the retained payload's complete size proof and workspace
/// policy. New payload measurement belongs on a supervised CPU worker. Neither
/// admission nor measurement performs database I/O.
pub struct GraphPublicationInput {
    identity: GraphRevision,
    payload: GraphPayload,
    changes: Option<ingest::FileChanges>,
    policy: GraphPolicy,
    retained_bytes: u64,
    run_excludes: Vec<String>,
    maximum_file_bytes: Option<usize>,
}

enum GraphPayload {
    Resident(Arc<CanonicalGenerationFacts>),
    Packed(packed::PackedGraph),
}

impl GraphPublicationInput {
    /// Actual retained input reservation, including source-admission allocations.
    #[must_use]
    pub fn retained_input_bytes(&self) -> u64 {
        self.retained_bytes
    }

    pub(in crate::rocks) fn reclaimable_bytes(&self) -> Result<u32> {
        match &self.payload {
            GraphPayload::Resident(_) => Ok(0),
            GraphPayload::Packed(graph) => {
                u32::try_from(graph.reclaimable_bytes()?).map_err(|_| limit())
            }
        }
    }

    /// Retain the already admitted canonical payload in the same project owner.
    /// No second copy of the file, reference, or document tables is allocated.
    #[must_use]
    pub fn shared_facts(&self) -> Option<Arc<CanonicalGenerationFacts>> {
        match &self.payload {
            GraphPayload::Resident(facts) => Some(facts.clone()),
            GraphPayload::Packed(_) => None,
        }
    }

    /// Validate a file-local replacement against an exact canonical base.
    /// The topology is retained; changes to declarations or resolution must use
    /// the complete native build instead. Publication fences both base identity
    /// and digest before applying the sparse records.
    /// # Errors
    /// Rejects changed identities, invalid relations, oversized work, or cancellation.
    pub fn replace_files(
        identity: GraphRevision,
        facts: CanonicalGenerationFacts,
        replacements: crate::GenerationFacts,
        policy: GraphPolicy,
        request: &StorageRequest,
    ) -> Result<Self> {
        request.check()?;
        let policy = policy.validate()?;
        if !matches!(identity.base, GraphBase::Current(_)) {
            return Err(RocksError::GenerationChanged);
        }
        let affected = replacements
            .files
            .iter()
            .map(|file| file.file_id.clone())
            .collect();
        let mut changes = ingest::FileChanges::capture(&facts, affected, policy, request)?;
        let limits = crate::GenerationValidationLimits::new(
            policy.maximum_input_bytes,
            policy
                .maximum_input_bytes
                .checked_mul(4)
                .ok_or_else(limit)?,
        )
        .map_err(|_| limit())?;
        let facts = crate::ingest::replace_file_facts(facts, replacements, limits, || {
            request.check().is_err()
        })
        .map_err(|error| match error {
            crate::GenerationValidationError::Cancelled => {
                request.check().err().unwrap_or(RocksError::Cancelled)
            }
            crate::GenerationValidationError::RetainedLimit => limit(),
            _ => RocksError::InvalidInput {
                field: "incremental_facts",
            },
        })?;
        changes.finish(&facts, policy, request)?;
        let mut input = Self::new(identity, facts, policy, request)?;
        input.retained_bytes = input
            .retained_bytes
            .checked_add(changes.retained_bytes()?)
            .ok_or_else(limit)?;
        if input.retained_bytes > policy.maximum_input_bytes {
            return Err(limit());
        }
        input.changes = Some(changes);
        Ok(input)
    }
    /// Exact canonical counts before the input moves to a storage worker.
    ///
    /// # Errors
    /// Rejects a count that cannot be represented in the durable manifest.
    pub fn info(&self) -> Result<GraphInfo> {
        let facts = match &self.payload {
            GraphPayload::Resident(facts) => facts,
            GraphPayload::Packed(graph) => return Ok(graph.info.clone()),
        };
        Ok(GraphInfo {
            format_version: 1,
            name_index_version: names::version(facts, self.policy),
            content_digest: facts.digest().clone(),
            files: count(facts.files().len())?,
            symbols: count(facts.symbols().len())?,
            edges: count(facts.edges().len())?,
            references: count(facts.references().len())?,
            numerical_sites: count(facts.numerical_sites().len())?,
            documents: count(facts.documents().len())?,
        })
    }

    /// Canonical source manifest for the owner's pre-publication source fence.
    #[must_use]
    pub fn files(&self) -> &[crate::FileInput] {
        match &self.payload {
            GraphPayload::Resident(facts) => facts.files(),
            GraphPayload::Packed(graph) => &graph.files,
        }
    }

    /// Prepare a validated generation inside the caller's existing CPU reservation.
    ///
    /// Large canonical tables are consumed into lossless bounded blocks before
    /// storage admission. Their original validation measurement and digest stay
    /// unchanged. Packed inputs do not retain a warm canonical allocation.
    /// # Errors
    /// Rejects preparation or retained-input bounds, invalid workspace,
    /// cancellation, and expired requests. No database I/O occurs here.
    pub fn prepare(
        identity: GraphRevision,
        facts: CanonicalGenerationFacts,
        policy: GraphPolicy,
        maximum_preparation_bytes: u64,
        request: &StorageRequest,
    ) -> Result<Self> {
        request.check()?;
        let policy = policy.validate()?;
        let measurement = facts
            .admission_measurement(maximum_preparation_bytes, || request.check().is_err())
            .map_err(|error| match error {
                GenerationMemoryModelError::Cancelled => {
                    request.check().err().unwrap_or(RocksError::Cancelled)
                }
                GenerationMemoryModelError::RetainedLimit => RocksError::Limit {
                    resource: "graph_preparation_bytes",
                },
                GenerationMemoryModelError::MetadataDepth => RocksError::Corrupt {
                    field: "graph_metadata",
                },
            })?;
        ingest::validate_workspace(&facts, policy, request)?;
        if measurement.retained_bytes() <= policy.maximum_input_bytes {
            return Self::new(identity, facts, policy, request);
        }
        let peak = measurement
            .retained_bytes()
            .checked_add(policy.maximum_input_bytes)
            .and_then(|bytes| bytes.checked_add(u64::try_from(policy.maximum_working_bytes).ok()?))
            .and_then(|bytes| bytes.checked_add(96 * 1_048_576))
            .ok_or_else(limit)?;
        if peak > maximum_preparation_bytes {
            return Err(RocksError::Limit {
                resource: "graph_preparation_bytes",
            });
        }
        let graph = packed::PackedGraph::build(facts, policy, request)?;
        let retained_bytes = graph.retained_bytes()?;
        if retained_bytes > policy.maximum_input_bytes {
            return Err(RocksError::Limit {
                resource: "graph_input_bytes",
            });
        }
        Ok(Self {
            identity,
            payload: GraphPayload::Packed(graph),
            changes: None,
            policy,
            retained_bytes,
            run_excludes: Vec::new(),
            maximum_file_bytes: None,
        })
    }

    /// Validate policy and use the immutable retained-size proof before async admission.
    ///
    /// # Errors
    /// Rejects excessive retained memory, cancellation, or expired requests.
    pub fn new(
        identity: GraphRevision,
        facts: CanonicalGenerationFacts,
        policy: GraphPolicy,
        request: &StorageRequest,
    ) -> Result<Self> {
        request.check()?;
        let policy = policy.validate()?;
        let measurement = facts
            .admission_measurement(policy.maximum_input_bytes, || request.check().is_err())
            .map_err(|error| match error {
                GenerationMemoryModelError::Cancelled => {
                    request.check().err().unwrap_or(RocksError::Cancelled)
                }
                GenerationMemoryModelError::RetainedLimit => RocksError::Limit {
                    resource: "graph_input_bytes",
                },
                GenerationMemoryModelError::MetadataDepth => RocksError::Corrupt {
                    field: "graph_metadata",
                },
            })?;
        request.check()?;
        ingest::validate_workspace(&facts, policy, request)?;
        Ok(Self {
            identity,
            payload: GraphPayload::Resident(Arc::new(facts)),
            changes: None,
            policy,
            retained_bytes: measurement.retained_bytes(),
            run_excludes: Vec::new(),
            maximum_file_bytes: None,
        })
    }

    /// Retain the exact run admission so later freshness checks use the same source set.
    /// # Errors
    /// Rejects excessive patterns, invalid text, or unsupported source-file bounds.
    pub fn with_source_admission(
        mut self,
        run_excludes: Vec<String>,
        maximum_file_bytes: Option<usize>,
    ) -> Result<Self> {
        let retained = |patterns: &Vec<String>| -> Option<usize> {
            patterns.iter().try_fold(
                patterns.capacity().checked_mul(size_of::<String>())?,
                |sum, pattern| sum.checked_add(pattern.capacity()),
            )
        };
        let bytes = retained(&run_excludes);
        if run_excludes.len() > 4096
            || bytes.is_none_or(|bytes| bytes > 4 * MIB)
            || run_excludes
                .iter()
                .any(|pattern| pattern.is_empty() || pattern.len() > 4096 || pattern.contains('\0'))
            || maximum_file_bytes.is_some_and(|bytes| bytes == 0 || bytes > 32 * MIB)
        {
            return Err(RocksError::InvalidInput {
                field: "source_admission",
            });
        }
        let old =
            u64::try_from(retained(&self.run_excludes).ok_or_else(limit)?).map_err(|_| limit())?;
        let added = u64::try_from(bytes.ok_or_else(limit)?).map_err(|_| limit())?;
        self.retained_bytes = self
            .retained_bytes
            .checked_sub(old)
            .and_then(|bytes| bytes.checked_add(added))
            .ok_or_else(limit)?;
        if self.retained_bytes > self.policy.maximum_input_bytes {
            return Err(limit());
        }
        self.run_excludes = run_excludes;
        self.maximum_file_bytes = maximum_file_bytes;
        Ok(self)
    }

    pub(super) fn reservation_bytes(&self) -> Result<u32> {
        // Includes old pages/records, new encoding, native batch copies, and
        // the retained canonical payload. No producer may enqueue without its
        // enclosing pipeline reservation while waiting for this admission.
        self.retained_bytes
            .checked_add(u64::try_from(self.policy.maximum_working_bytes).map_err(|_| limit())?)
            .and_then(|bytes| bytes.checked_add(96 * 1_048_576))
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(self.policy.maximum_batch_bytes)
                        .ok()?
                        .checked_mul(4)?,
                )
            })
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(limit)
    }
}

/// Exact canonical row counts and physical graph-format identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphInfo {
    /// Version of the typed records and compact adjacency representation.
    pub format_version: u8,
    /// Exact-name postings format; zero requires bounded scan fallback.
    /// Older manifests omit this optional derived capability.
    #[serde(default)]
    pub name_index_version: u8,
    /// Complete canonical digest, independent of physical adjacency encoding.
    pub content_digest: ContentDigest,
    /// Source files in this complete revision.
    pub files: u64,
    /// Declared symbols in this complete revision.
    pub symbols: u64,
    /// Canonical relationships, preserving distinct provenance.
    pub edges: u64,
    /// Resolved and unresolved source-reference rows.
    pub references: u64,
    /// Static numerical evidence rows.
    pub numerical_sites: u64,
    /// Durable canonical documents; a native lexical index is a separate capability.
    pub documents: u64,
}

/// Fixed-size accounting for one complete incremental replacement.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct GraphWriteReport {
    /// Candidate canonical records and packed adjacency blocks compared.
    pub candidate_records: u64,
    /// Existing equal records requiring no write or staging claim.
    pub reused_records: u64,
    /// New or changed logical values staged durably.
    pub written_records: u64,
    /// Removed records and obsolete adjacency blocks tombstoned.
    pub deleted_records: u64,
    /// Packed adjacency blocks in both directions.
    pub adjacency_blocks: u64,
    /// Largest number of canonical relationships adjacent to a symbol.
    pub maximum_degree: u64,
}

/// Retention is reported independently from an already committed publication.
pub enum GraphRetention {
    /// One bounded resumable cleanup batch completed.
    Completed(PruneReport),
    /// The graph committed; maintenance needs a later bounded attempt.
    Deferred(RocksError),
}

/// A complete durable graph revision and its incremental-write accounting.
pub struct GraphPublication {
    /// Actual committed current revision.
    pub revision: StoredRevision,
    /// Comparison and adjacency accounting.
    pub report: GraphWriteReport,
    /// Bounded maintenance outcome; never changes publication success.
    pub retention: GraphRetention,
}

/// Primary publication failure with independently reported cleanup diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{primary}")]
pub struct GraphPublicationError {
    /// The original request, fence, validation, or storage failure.
    #[source]
    pub primary: RocksError,
    /// Cleanup could not finish; this never replaces the primary failure.
    pub cleanup_failure: Option<RocksError>,
}

impl From<RocksError> for GraphPublicationError {
    fn from(primary: RocksError) -> Self {
        Self {
            primary,
            cleanup_failure: None,
        }
    }
}

fn limit() -> RocksError {
    RocksError::Limit {
        resource: "graph_working_bytes",
    }
}

fn count(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| limit())
}

fn check_workspace<T>(items: usize, per_item_bytes: usize, policy: GraphPolicy) -> Result<()> {
    let bytes = items
        .checked_mul(size_of::<T>().saturating_add(per_item_bytes))
        .ok_or_else(limit)?;
    if bytes > policy.maximum_working_bytes {
        return Err(limit());
    }
    Ok(())
}

impl ProjectStore {
    /// Replace a complete typed graph using only changed records and blocks.
    ///
    /// The caller supplies complete canonical facts from the existing native
    /// resolver, including cross-file changes. This synchronous method belongs
    /// on a storage worker. Readers continue using the old complete snapshot
    /// until all records and both adjacency directions are durably published.
    ///
    /// # Errors
    /// Rejects a stale base, format mismatch, bound, request, or storage failure.
    /// Incomplete staged work stays invisible. The primary failure and any
    /// secondary cleanup failure are reported independently.
    pub fn publish_graph(
        &self,
        input: GraphPublicationInput,
        request: &StorageRequest,
    ) -> std::result::Result<GraphPublication, GraphPublicationError> {
        self.publish_graph_reclaiming(input, request, |_| Ok(()))
    }

    pub(in crate::rocks) fn publish_graph_reclaiming(
        &self,
        input: GraphPublicationInput,
        request: &StorageRequest,
        reclaim: impl FnMut(usize) -> Result<()>,
    ) -> std::result::Result<GraphPublication, GraphPublicationError> {
        ingest::publish(self, input, request, reclaim)
    }
}
