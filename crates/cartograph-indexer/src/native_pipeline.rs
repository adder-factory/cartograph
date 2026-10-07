mod codeigniter_resolution;
mod component_imports;
mod csharp_constructors;
mod declaration_resolution;
mod drupal_resolution;
mod drupal_tags;
mod enum_resolution;
mod explicit_edge_resolution;
mod file_path_resolution;
mod framework_conventions;
mod framework_imports;
mod framework_methods;
mod framework_provided;
mod framework_resolution;
mod generic_resolution;
mod go_module_paths;
mod go_path_resolution;
mod intrinsic_names;
mod javascript_alias_exports;
mod javascript_config;
mod javascript_exports;
mod javascript_framework_resolution;
mod javascript_member_resolution;
mod javascript_modules;
mod javascript_packages;
mod jvm_nested_resolution;
mod jvm_resolution;
mod module_call_resolution;
mod namespace_types;
mod nominal_scope_resolution;
mod ocaml_module_resolution;
mod pascal_resolution;
mod php_resolution;
mod play_resolution;
mod python_class_members;
mod python_resolution;
mod python_type_variables;
mod qualified_member_resolution;
mod qualtype_generics;
mod qualtype_resolution;
mod qualtype_source;
mod receiver_resolution;
mod reference_dispatch;
mod reference_tiers;
mod repr_file_imports;
mod rescript_resolution;
mod resource_resolution;
mod rust_dependency_paths;
mod rust_facade_resolution;
mod rust_inline_modules;
mod rust_local_types;
mod rust_path_guards;
mod rust_path_resolution;
mod rust_path_visibility;
mod rust_root_ownership;
mod rust_scoped_modules;
mod rust_uniform_paths;
mod rust_use_bindings;
mod salesforce_resolution;
mod scip_spill;
mod script_modules;
mod shell_resolution;

use intrinsic_names::BuiltinVocabulary::{PythonExceptions, PythonValues};

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    iter::Peekable,
    mem::{size_of, take},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use cartograph_db::{
    CachedRowPayload, CanonicalGenerationFacts, CartographDatabase, EdgeInput, FactBatchInput,
    FileInput, GenerationFacts, GenerationMemoryModelError, GenerationValidationError,
    GenerationValidationLimits, GenerationValidationReport, MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES,
    NativeGenerationExtractedCursor, NativeGenerationExtractedPage, NativeGenerationSpill,
    NativeGenerationSpillCachedRow, NativeGenerationSpillCentralityScore,
    NativeGenerationSpillDigestReport, NativeGenerationSpillExtractedBatch,
    NativeGenerationSpillExtractedRow, NativeGenerationSpillFactBatch,
    NativeGenerationSpillFactCounts, NativeGenerationSpillReport, NativeGenerationSpillRow,
    NativeGenerationSpillState, NativeParseCacheEntry, NativeParseCacheKey,
    NativeParseCacheKeyInput, NativeParseCacheRecord, NativeParseCacheWrite, NumericalSiteInput,
    ReferenceInput, ReferenceSpanPrecision, SearchDocumentInput, StorageError, SymbolInput,
    apply_page_rank, apply_sampled_betweenness, validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, NormalizedPath, ProjectId,
    ReferenceKind, SourceLanguage, SourceSpan, SymbolExecutionFlags, SymbolExportFlags, SymbolId,
    SymbolImplementationFlags, SymbolKind, Visibility, symbol_signature_is_search_safe,
};
use cartograph_extract::{
    CloneTokenCount, CloneTokenProfile, Containment, DEFAULT_MAXIMUM_AST_DEPTH,
    DYNAMIC_DISPATCH_RESOLUTION_PREFIX, DeclarationSyntax, DiagnosticCode, DiscoveredSource,
    DiscoveryLimits, EMBEDDED_SQL_RESOLUTION_PREFIX, ExtractError, ExtractedCallScopeSite,
    ExtractedFile, ExtractedImportBinding, ExtractedNumericalSite, ExtractedReceiverEvidence,
    ExtractedReference, ImportBindingKind, JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX,
    JavascriptMemberCallContext, JavascriptMemberReceiver, LEXICAL_SCOPE_RESOLUTION_PREFIX,
    MAXIMUM_AST_DEPTH, MINIMUM_AST_DEPTH, NativeExtractor, PHP_EXACT_RESOLUTION_PREFIX,
    PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX, RUST_MACRO_RESOLUTION_PREFIX,
    RUST_SELF_RECEIVER_RESOLUTION_PREFIX, SourceDiscoveryOptions, SourceExclusionEvidence,
    SourceLimits, SourceReadError, SourceReadOptions, SourceRoot, SourceSnapshot,
    TYPE_QUERY_VALUE_RESOLUTION_PREFIX, is_test_source_path, native_extraction_reservation,
    native_extractor_contract_digest, native_read_reservation, substitute_module_alias,
};
use cartograph_scip::{
    ScipOverlayPlan, ScipOverlayReport, ScipOverlayRequest, apply_scip_overlay_with_cancellation,
};
use globset::GlobBuilder;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    runtime::{Handle, RuntimeFlavor},
    task::{JoinSet, block_in_place},
    time::Instant,
};

use crate::{
    PipelineFailureReason, PipelineFileFailure, PipelineStage, StageCancellation, StageCapacity,
    StageDeadlinePolicy, StageEnvelope, StageExecution, StageFailureKind, StageFold,
    StageItemBudget, StageItemFailure, StageItemMeta, StageOutput, StageRunConfig, StageRunError,
    StageRunner, StageSequence, StageWorkItem, StageWorkload,
};

const MAX_PIPELINE_RETAINED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_STAGE_TIMEOUT: Duration = Duration::from_hours(24);
const MAX_CLEANUP_GRACE: Duration = Duration::from_mins(1);
const MAX_REPORTED_DEGRADED_FILES: usize = 32;
const DOCUMENT_ID_DOMAIN: &[u8] = b"cartograph-v2-native-document-v1";
const MYBATIS_BRIDGE_PROVENANCE: &str = "framework-mybatis-qualified";
const NATIVE_MODULE_BRIDGE_PROVENANCE: &str = "framework-native-module-impl";
const NATIVE_EVENT_BRIDGE_PROVENANCE: &str = "framework-native-event-channel";
const FABRIC_NATIVE_BRIDGE_PROVENANCE: &str = "framework-fabric-native-impl";
const TURBO_NATIVE_BRIDGE_PROVENANCE: &str = "framework-turbo-native-impl";
const DRUPAL_TAG_PROVIDES_PROVENANCE: &str = "framework-drupal-tag-provides";
const DRUPAL_TAG_CONSUMES_PROVENANCE: &str = "framework-drupal-tag-consumes";
const DRUPAL_TAG_EVIDENCE_PROVENANCE: &str = "framework-drupal-tag-evidence";
const MANIFEST_WORKSPACE_PROVENANCE: &str = "framework-manifest-workspace-member";
const APPLE_BRIDGE_PROVENANCE: &str = "framework-swift-objc-bridge";
const FILE_SYMBOL_ID_DOMAIN: &[u8] = b"cartograph-v2-native-file-symbol-v1";
const CONTAINMENT_PROVENANCE: &str = "native-tree-sitter-containment";
const FILE_CONTAINMENT_PROVENANCE: &str = "native-file-containment";
const EXACT_LEXICAL_PROVENANCE: &str = "native-exact-lexical";
const IMPORT_BINDING_PROVENANCE: &str = "native-import-binding";
const MODULE_IMPORT_PROVENANCE: &str = "native-module-import";
const QUOTED_INCLUDE_PROVENANCE: &str = "native-c-quoted-include";
const EXACT_SAME_FILE_PROVENANCE: &str = "native-exact-same-file";
const EXACT_PROJECT_PROVENANCE: &str = "native-exact-project";
const RUST_QUALIFIED_PATH_PROVENANCE: &str = "native-rust-qualified-path";
const RUST_WORKSPACE_CRATE_PROVENANCE: &str = "native-rust-workspace-crate";
const FRAMEWORK_CONVENTION_PROVENANCE: &str = "native-framework-convention";
const DYNAMIC_DISPATCH_PROVENANCE: &str = "native-dynamic-dispatch";
const DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE: &str = "native-dynamic-unresolved";
const MEMBER_ACCESS_UNRESOLVED_PROVENANCE: &str = "native-member-unresolved";
const RUST_EXTERNAL_UNRESOLVED_PROVENANCE: &str = "native-rust-external";
const RUST_INTRINSIC_UNRESOLVED_PROVENANCE: &str = "native-rust-intrinsic";
const RUST_MACRO_UNRESOLVED_PROVENANCE: &str = "native-rust-macro-unexpanded";
const EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE: &str = "native-external-reference";
const JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE: &str = "native-javascript-intrinsic";
const PYTHON_INTRINSIC_UNRESOLVED_PROVENANCE: &str = "native-python-intrinsic";
const SHELL_COMMAND_UNRESOLVED_PROVENANCE: &str = "native-shell-command";
const MANIFEST_REFERENCE_UNRESOLVED_PROVENANCE: &str = "native-manifest-reference";
const EMBEDDED_SQL_READ_PROVENANCE: &str = "native-embedded-sql-read";
const EMBEDDED_SQL_WRITE_PROVENANCE: &str = "native-embedded-sql-write";
const EMBEDDED_SQL_DDL_PROVENANCE: &str = "native-embedded-sql-ddl";
const EMBEDDED_SQL_READ_UNRESOLVED: &str = "native-embedded-sql-read-unresolved";
const EMBEDDED_SQL_WRITE_UNRESOLVED: &str = "native-embedded-sql-write-unresolved";
const EMBEDDED_SQL_DDL_UNRESOLVED: &str = "native-embedded-sql-ddl-unresolved";
const GO_RECEIVER_OWNERSHIP_PROVENANCE: &str = "native-go-receiver-ownership";
const GO_IMPLEMENTS_PROVENANCE: &str = "native-go-structural-implements";
const RE_EXPORT_ALL_PROVENANCE: &str = "native-module-re-export-all";
const RE_EXPORT_NAMESPACE_PROVENANCE: &str = "native-module-re-export-namespace";
const TEST_CONVENTION_PROVENANCE: &str = "native-test-subject-convention";
const TEST_IMPORT_PROVENANCE: &str = "native-test-subject-import";
const RUST_INTEGRATION_TEST_PROVENANCE: &str = "native-rust-integration-test";
const RUST_INLINE_TEST_PROVENANCE: &str = "native-rust-inline-test";
const UNRESOLVED_IMPORT_PROVENANCE: &str = "native-unresolved-import";
const UNRESOLVED_PROVENANCE: &str = "native-unresolved";
const EXACT_LEXICAL_CONFIDENCE: f32 = 1.0;
const IMPORT_BINDING_CONFIDENCE: f32 = 1.0;
const EXACT_SAME_FILE_CONFIDENCE: f32 = 1.0;
const EXACT_PROJECT_CONFIDENCE: f32 = 0.95;
const FRAMEWORK_CONVENTION_CONFIDENCE: f32 = 0.85;
const DYNAMIC_DISPATCH_CONFIDENCE: f32 = 0.65;
const EMBEDDED_SQL_CONFIDENCE: f32 = 0.90;
const GO_STRUCTURAL_CONFIDENCE: f32 = 0.95;
const TEST_CONVENTION_CONFIDENCE: f32 = 0.95;
const TEST_IMPORT_CONFIDENCE: f32 = 0.90;
const EXTRACTED_EDGE_CONFIDENCE: f32 = 1.0;
const UNRESOLVED_CONFIDENCE: f32 = 0.0;
const DOCUMENT_METADATA_FIXED_ALLOWANCE: u64 = 2 * 1024;
const DOCUMENT_UUID_BYTES: usize = 16;
const UUID_TEXT_BYTES: u64 = 36;
const MAX_RESOLUTION_PROVENANCE_BYTES: u64 = 64;
const MAX_SYMBOL_QUALIFIED_NAME_BYTES: usize = 2_048;
const FILE_SYMBOL_FALLBACK_PREFIX: &str = "file:";
const RESOLVE_WORKING_MULTIPLIER: u64 = 4;
const VALIDATION_WORKING_MULTIPLIER: u64 = 4;
const MAXIMUM_SCIP_OVERLAY_BYTES: usize = 256 * 1024 * 1024;
const MAXIMUM_SCIP_OVERLAY_ROWS: usize = 10_000_000;
const RESOLUTION_MAP_NODE_ALLOWANCE: u64 = 128;
const DUPLICATE_MINIMUM_LINES: u32 = 6;
const DUPLICATE_NEAR_MINIMUM_LINES: u32 = 12;
const DUPLICATE_MAXIMUM_LITERAL_RATIO_PPM: u64 = 600_000;
const DUPLICATE_PARTIAL_DEFAULT_OVERLAP_PPM: u32 = 950_000;
const DUPLICATE_PARTIAL_WIDER_OVERLAP_PPM: u32 = 800_000;
const DUPLICATE_IDENTIFIER_MINIMUM_OVERLAP_PPM: u32 = 500_000;
const DUPLICATE_SAME_FILE_IDENTIFIER_OVERLAP_PPM: u32 = 100_000;
const CLONE_PREFILTER_BUCKETS: usize = 16;
const CLONE_PREFILTER_SHIFT: u32 = 60;
const MAXIMUM_LISTED_CLONE_PEERS: usize = 10;
const MAXIMUM_TYPESCRIPT_ALIAS_CONFIGS: usize = 256;
const MAXIMUM_TYPESCRIPT_PATH_MAPPINGS: usize = 256;
const MAXIMUM_TYPESCRIPT_PATH_SUBSTITUTIONS: usize = 32;
const MAXIMUM_TYPESCRIPT_PATH_TEXT_BYTES: usize = 4_096;
const SPILLED_EXTRACTION_PAGE_BYTES: u64 = 32 * 1024 * 1024;
const SPILLED_PARSE_BATCH_FILES: usize = 64;
const SPILLED_PARSE_BATCH_BYTES: u64 = 64 * 1024 * 1024;
const SPILLED_PARSE_BATCH_RESERVATION_BYTES: u64 = 64 * 1024 * 1024;
const SPILLED_PARSE_CACHE_WRITE_BYTES: usize = 64 * 1024 * 1024;
const SPILLED_RESOLVE_TRANSACTION_BATCHES: usize = 256;
const SPILLED_RESOLVE_TRANSACTION_BYTES: u64 = 64 * 1024 * 1024;
const SPILLED_RESOLVE_TRANSACTION_ROWS: u64 = 200_000;
const SPILLED_RESOLVE_TRANSACTION_RETAINED_BYTES: u64 = 512 * 1024 * 1024;
const SPILLED_REDUCTION_RESERVATION_BYTES: u64 = 64 * 1024 * 1024;
const SPILLED_CENTRALITY_UPDATE_ROWS: usize = 100_000;
const MAXIMUM_PROJECT_NON_VISIBLE_LANGUAGES: usize = 12;
/// PostgreSQL-backed path/content cache policy for one complete native build.
#[derive(Clone)]
pub struct NativeParseCache {
    database: CartographDatabase,
    project_id: ProjectId,
    extractor_contract_digest: ContentDigest,
    read_enabled: bool,
}

impl fmt::Debug for NativeParseCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeParseCache")
            .field("project_id", &self.project_id)
            .field("extractor_contract_digest", &self.extractor_contract_digest)
            .field("read_enabled", &self.read_enabled)
            .finish_non_exhaustive()
    }
}

impl NativeParseCache {
    /// Use the current source-derived extractor contract and permit exact cache reads.
    #[must_use]
    pub fn new(database: CartographDatabase, project_id: ProjectId) -> Self {
        Self {
            database,
            project_id,
            extractor_contract_digest: native_extractor_contract_digest(),
            read_enabled: true,
        }
    }

    /// Force native reparsing while still validating/populating immutable cache rows.
    #[must_use]
    pub const fn with_reads(mut self, enabled: bool) -> Self {
        self.read_enabled = enabled;
        self
    }

    /// Override the contract only for compatibility verification and migration tests.
    #[doc(hidden)]
    #[must_use]
    pub fn with_contract_digest(mut self, digest: ContentDigest) -> Self {
        self.extractor_contract_digest = digest;
        self
    }

    fn with_maximum_ast_depth(mut self, maximum_ast_depth: usize) -> Self {
        self.extractor_contract_digest =
            parse_cache_policy_digest(&self.extractor_contract_digest, maximum_ast_depth);
        self
    }

    fn key(&self, manifest: &SourceManifestEntry) -> NativeParseCacheKey {
        NativeParseCacheKey::new(NativeParseCacheKeyInput {
            project_id: self.project_id.clone(),
            extractor_contract_digest: self.extractor_contract_digest.clone(),
            path: manifest.path.clone(),
            language: manifest.language,
            content_hash: manifest.content_hash.clone(),
            source_bytes: manifest.byte_size,
        })
    }
}

/// Exact current parse-cache identity, including the admitted AST depth.
#[must_use]
pub fn native_parse_cache_contract_digest(maximum_ast_depth: usize) -> ContentDigest {
    parse_cache_policy_digest(&native_extractor_contract_digest(), maximum_ast_depth)
}

fn parse_cache_policy_digest(
    extractor_contract_digest: &ContentDigest,
    maximum_ast_depth: usize,
) -> ContentDigest {
    let mut hasher =
        blake3::Hasher::new_derive_key("cartograph.v2.native-parse-cache-policy.2026-08-13");
    hasher.update(extractor_contract_digest.as_str().as_bytes());
    hasher.update(
        &u64::try_from(maximum_ast_depth)
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

/// Hard discovery, source, manifest, and canonical-generation memory bounds.
#[derive(Clone, Copy, Debug)]
pub struct NativePipelineLimits {
    discovery_limits: DiscoveryLimits,
    source_limits: SourceLimits,
    retained: NativeRetainedLimits,
}

/// Separately bounded compact manifest and canonical generation storage.
#[derive(Clone, Copy, Debug)]
pub struct NativeRetainedLimits {
    max_manifest_bytes: u64,
    max_generation_bytes: u64,
}

impl NativeRetainedLimits {
    /// Validate both project-retained memory ceilings.
    /// # Errors
    ///
    /// Returns an error if either retained-memory ceiling is zero or exceeds
    /// the native pipeline hard maximum.
    pub fn new(
        max_manifest_bytes: u64,
        max_generation_bytes: u64,
    ) -> Result<Self, NativePipelineConfigError> {
        validate_retained_limit(max_manifest_bytes, "max_manifest_bytes")?;
        validate_retained_limit(max_generation_bytes, "max_generation_bytes")?;
        Ok(Self {
            max_manifest_bytes,
            max_generation_bytes,
        })
    }
}

impl NativePipelineLimits {
    /// Combine validated discovery, source, and retained-memory policies.
    #[must_use]
    pub const fn new(
        discovery_limits: DiscoveryLimits,
        source_limits: SourceLimits,
        retained: NativeRetainedLimits,
    ) -> Self {
        Self {
            discovery_limits,
            source_limits,
            retained,
        }
    }
}

/// Independent bounded worker windows for streamed reads and native parsing.
#[derive(Clone, Copy, Debug)]
pub struct NativePipelineParallelism {
    read_capacity: StageCapacity,
    parse_capacity: StageCapacity,
}

impl NativePipelineParallelism {
    /// Validate both active-worker plus queue windows.
    /// # Errors
    ///
    /// Returns an error if either worker count is zero or its active-plus-queued
    /// window overflows the bounded stage capacity.
    pub fn new(
        read_capacity: StageCapacity,
        parse_capacity: StageCapacity,
    ) -> Result<Self, NativePipelineConfigError> {
        validate_capacity(read_capacity, "read_capacity")?;
        validate_capacity(parse_capacity, "parse_capacity")?;
        Ok(Self {
            read_capacity,
            parse_capacity,
        })
    }
}

/// Per-item, whole-stage, and post-cancellation cleanup bounds.
#[derive(Clone, Copy, Debug)]
pub struct NativePipelineDeadlines {
    item_timeout: Duration,
    stage_timeout: Duration,
    cleanup_grace: Duration,
}

impl NativePipelineDeadlines {
    /// Validate independent work, stage, and cleanup durations.
    /// # Errors
    ///
    /// Returns an error if any duration is zero or a stage/item timeout or
    /// cleanup grace exceeds its independent hard maximum.
    pub fn new(
        item_timeout: Duration,
        stage_timeout: Duration,
        cleanup_grace: Duration,
    ) -> Result<Self, NativePipelineConfigError> {
        if item_timeout.is_zero() || item_timeout > MAX_STAGE_TIMEOUT {
            return Err(NativePipelineConfigError::invalid("item_timeout"));
        }
        if stage_timeout.is_zero() || stage_timeout > MAX_STAGE_TIMEOUT {
            return Err(NativePipelineConfigError::invalid("stage_timeout"));
        }
        if cleanup_grace.is_zero() || cleanup_grace > MAX_CLEANUP_GRACE {
            return Err(NativePipelineConfigError::invalid("cleanup_grace"));
        }
        Ok(Self {
            item_timeout,
            stage_timeout,
            cleanup_grace,
        })
    }
}

/// Complete validated policy for one native source-to-facts build.
#[derive(Clone, Copy, Debug)]
pub struct NativePipelineConfig {
    limits: NativePipelineLimits,
    parallelism: NativePipelineParallelism,
    deadlines: NativePipelineDeadlines,
    evidence: NativeEvidencePolicy,
    clones: NativeClonePolicy,
    maximum_ast_depth: usize,
}

#[derive(Clone, Copy, Debug)]
struct NativeEvidencePolicy {
    centrality: NativeCentralityPolicy,
    retention: NativeRetentionPolicy,
}

#[derive(Clone, Copy, Debug)]
struct NativeCentralityPolicy {
    page_rank: bool,
    betweenness: bool,
}

#[derive(Clone, Copy, Debug)]
struct NativeRetentionPolicy {
    docstrings: bool,
    call_sites: bool,
}

#[derive(Clone, Copy, Debug)]
struct NativeClonePolicy {
    wider_partial_band: bool,
}

impl NativeEvidencePolicy {
    const FULL: Self = Self {
        centrality: NativeCentralityPolicy {
            page_rank: true,
            betweenness: true,
        },
        retention: NativeRetentionPolicy {
            docstrings: true,
            call_sites: true,
        },
    };

    #[cfg(test)]
    const STRUCTURAL: Self = Self {
        centrality: NativeCentralityPolicy {
            page_rank: true,
            betweenness: true,
        },
        retention: NativeRetentionPolicy {
            docstrings: false,
            call_sites: false,
        },
    };
}

/// Bounded SCIP protobuf bytes applied as a persistent per-file overlay.
pub struct ScipOverlayInput {
    bytes: Vec<u8>,
    maximum_rows: usize,
    content_digest: ContentDigest,
}

impl fmt::Debug for ScipOverlayInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScipOverlayInput")
            .field("bytes", &self.bytes.len())
            .field("maximum_rows", &self.maximum_rows)
            .field("content_digest", &self.content_digest)
            .finish()
    }
}

impl ScipOverlayInput {
    /// Validate the artifact and decoded-row ceiling before stage admission.
    /// # Errors
    ///
    /// Returns an error if the SCIP artifact is empty/oversized or
    /// `maximum_rows` is zero or above the overlay row ceiling.
    pub fn new(bytes: Vec<u8>, maximum_rows: usize) -> Result<Self, NativePipelineConfigError> {
        if bytes.is_empty()
            || bytes.len() > MAXIMUM_SCIP_OVERLAY_BYTES
            || maximum_rows == 0
            || maximum_rows > MAXIMUM_SCIP_OVERLAY_ROWS
        {
            return Err(NativePipelineConfigError::invalid("scip_overlay"));
        }
        let content_digest = ContentDigest::from_bytes(*blake3::hash(&bytes).as_bytes());
        Ok(Self {
            bytes,
            maximum_rows,
            content_digest,
        })
    }

    /// Exact encoded artifact size admitted to the overlay stage.
    #[must_use]
    pub fn byte_size(&self) -> usize {
        self.bytes.len()
    }

    /// Maximum decoded documents, symbols, and occurrences.
    #[must_use]
    pub const fn maximum_rows(&self) -> usize {
        self.maximum_rows
    }

    /// Digest of the exact immutable artifact bytes used by freshness identity.
    #[must_use]
    pub const fn content_digest(&self) -> &ContentDigest {
        &self.content_digest
    }
}

impl NativePipelineConfig {
    /// Combine already validated memory, parallelism, and deadline policies.
    #[must_use]
    pub const fn new(
        limits: NativePipelineLimits,
        parallelism: NativePipelineParallelism,
        deadlines: NativePipelineDeadlines,
    ) -> Self {
        Self {
            limits,
            parallelism,
            deadlines,
            evidence: NativeEvidencePolicy::FULL,
            clones: NativeClonePolicy {
                wider_partial_band: false,
            },
            maximum_ast_depth: DEFAULT_MAXIMUM_AST_DEPTH,
        }
    }

    /// Retain the v1 `enableCentrality` contract for native `PageRank`.
    #[must_use]
    pub const fn with_page_rank(mut self, enabled: bool) -> Self {
        self.evidence.centrality.page_rank = enabled;
        self
    }

    /// Retain the v1 `enableBetweenness` override for bounded native Brandes.
    #[must_use]
    pub const fn with_betweenness(mut self, enabled: bool) -> Self {
        self.evidence.centrality.betweenness = enabled;
        self
    }

    /// Retain or omit extracted documentation text while preserving symbols.
    #[must_use]
    pub const fn with_docstrings(mut self, enabled: bool) -> Self {
        self.evidence.retention.docstrings = enabled;
        self
    }

    /// Retain exact/coarse reference sites while preserving aggregated graph edges.
    #[must_use]
    pub const fn with_call_sites(mut self, enabled: bool) -> Self {
        self.evidence.retention.call_sites = enabled;
        self
    }

    /// Add the opt-in v1 0.80 Type-3 clone band below the always-on 0.95 band.
    #[must_use]
    pub const fn with_partial_clones(mut self, enabled: bool) -> Self {
        self.clones.wider_partial_band = enabled;
        self
    }

    /// Apply the project-specific AST depth within the extractor hard safety range.
    /// # Errors
    ///
    /// Returns an error outside 64..=1024.
    pub fn with_maximum_ast_depth(
        mut self,
        value: usize,
    ) -> Result<Self, NativePipelineConfigError> {
        if !(MINIMUM_AST_DEPTH..=MAXIMUM_AST_DEPTH).contains(&value) {
            return Err(NativePipelineConfigError::invalid("maximum_ast_depth"));
        }
        self.maximum_ast_depth = value;
        Ok(self)
    }

    /// Largest single supervisor-owned worker reservation required by this pipeline.
    /// # Errors
    ///
    /// Returns an error if a retained generation ceiling cannot be expanded
    /// into the separately bounded resolve and validation working allowances.
    pub fn maximum_stage_reservation_bytes(self) -> Result<u64, NativePipelineConfigError> {
        let retained = self.limits.retained.max_generation_bytes;
        let resolve = retained
            .checked_mul(RESOLVE_WORKING_MULTIPLIER)
            .ok_or_else(|| NativePipelineConfigError::invalid("max_generation_bytes"))?;
        let validation = retained
            .checked_mul(VALIDATION_WORKING_MULTIPLIER)
            .ok_or_else(|| NativePipelineConfigError::invalid("max_generation_bytes"))?;
        Ok(resolve.max(validation))
    }

    fn stage_deadlines(self) -> StageDeadlinePolicy {
        let now = Instant::now();
        StageDeadlinePolicy::new(
            now.checked_add(self.deadlines.stage_timeout).unwrap_or(now),
            self.deadlines.cleanup_grace,
        )
    }

    const fn evidence_policy(self) -> NativeEvidencePolicy {
        self.evidence
    }

    const fn clone_policy(self) -> NativeClonePolicy {
        self.clones
    }

    const fn maximum_ast_depth(self) -> usize {
        self.maximum_ast_depth
    }
}

/// A native pipeline policy was zero, overflowed, or exceeded a hard ceiling.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("invalid {field} in native pipeline configuration")]
pub struct NativePipelineConfigError {
    /// Stable field name without project data.
    pub field: &'static str,
}

impl NativePipelineConfigError {
    const fn invalid(field: &'static str) -> Self {
        Self { field }
    }
}

/// Fixed-size accounting for one complete native source-to-facts build.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativePipelineReport {
    discovered_files: u64,
    excluded_paths: u64,
    excluded_trees: u64,
    skipped_oversized_files: u64,
    source_bytes: u64,
    symbols: u64,
    numerical_sites: u64,
    resolved_references: u64,
    unresolved_references: u64,
    diagnostics: u64,
    degraded_files: Vec<NativeDegradedFile>,
    degraded_files_truncated: u64,
    modeled_generation_bytes: u64,
    resolve_high_water_bytes: u64,
    validation_high_water_bytes: u64,
    scip_overlay: Option<ScipOverlayReport>,
    overlay_high_water_bytes: u64,
    parse_cache: NativeParseCacheReport,
    storage: NativeGenerationStorage,
    spill: Option<NativeGenerationSpillReport>,
}

/// One source file retained as partial after a recoverable extraction boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeDegradedFile {
    path: String,
    reason: &'static str,
}

impl NativeDegradedFile {
    /// Exact normalized project-relative source path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Stable source-safe recovery category.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.reason
    }
}

/// Physical working-set strategy used before atomic generation publication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NativeGenerationStorage {
    /// Retain and canonically reduce one complete native generation in Rust memory.
    #[default]
    Memory,
    /// Spill file-local and resolved facts behind the exact staging fence in PostgreSQL.
    PostgreSql,
}

/// Exact per-file incremental parsing evidence for one complete graph build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeParseCacheReport {
    hits: u64,
    misses: u64,
    bypassed: u64,
    parsed_files: u64,
    writes: u64,
    corruptions: u64,
    read_errors: u64,
    write_errors: u64,
}

impl NativeParseCacheReport {
    #[must_use]
    /// Returns the hits.
    pub const fn hits(self) -> u64 {
        self.hits
    }

    #[must_use]
    /// Returns the misses.
    pub const fn misses(self) -> u64 {
        self.misses
    }

    #[must_use]
    /// Returns the bypassed.
    pub const fn bypassed(self) -> u64 {
        self.bypassed
    }

    #[must_use]
    /// Returns the parsed files.
    pub const fn parsed_files(self) -> u64 {
        self.parsed_files
    }

    #[must_use]
    /// Returns the writes.
    pub const fn writes(self) -> u64 {
        self.writes
    }

    #[must_use]
    /// Returns the corruptions.
    pub const fn corruptions(self) -> u64 {
        self.corruptions
    }

    #[must_use]
    /// Reads the errors from the authoritative store.
    pub const fn read_errors(self) -> u64 {
        self.read_errors
    }

    #[must_use]
    /// Persists the errors in the authoritative store.
    pub const fn write_errors(self) -> u64 {
        self.write_errors
    }

    fn add(&mut self, item: Self) {
        self.hits = self.hits.saturating_add(item.hits);
        self.misses = self.misses.saturating_add(item.misses);
        self.bypassed = self.bypassed.saturating_add(item.bypassed);
        self.parsed_files = self.parsed_files.saturating_add(item.parsed_files);
        self.writes = self.writes.saturating_add(item.writes);
        self.corruptions = self.corruptions.saturating_add(item.corruptions);
        self.read_errors = self.read_errors.saturating_add(item.read_errors);
        self.write_errors = self.write_errors.saturating_add(item.write_errors);
    }
}

impl NativePipelineReport {
    /// Supported source files admitted by discovery.
    #[must_use]
    pub const fn discovered_files(&self) -> u64 {
        self.discovered_files
    }

    /// Paths a configured exclusion glob or `.cartographignore` pattern skipped.
    ///
    /// A partial index must never look complete, so this is reported even when
    /// the exclusion was exactly what the project asked for.
    #[must_use]
    pub const fn excluded_paths(&self) -> u64 {
        self.excluded_paths
    }

    /// Directory subtrees skipped without being descended into.
    #[must_use]
    pub const fn excluded_trees(&self) -> u64 {
        self.excluded_trees
    }

    /// Supported files excluded before reads because their observed size exceeded policy.
    #[must_use]
    pub const fn skipped_oversized_files(&self) -> u64 {
        self.skipped_oversized_files
    }

    /// Exact source bytes hashed and then revalidated before parsing.
    #[must_use]
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }

    /// Native declarations emitted into the generation.
    #[must_use]
    pub const fn symbols(&self) -> u64 {
        self.symbols
    }

    /// Static numerical source sites emitted into the generation.
    #[must_use]
    pub const fn numerical_sites(&self) -> u64 {
        self.numerical_sites
    }

    /// References assigned one deterministic project symbol.
    #[must_use]
    pub const fn resolved_references(&self) -> u64 {
        self.resolved_references
    }

    /// Explicit unresolved reference rows retained for later improvements.
    #[must_use]
    pub const fn unresolved_references(&self) -> u64 {
        self.unresolved_references
    }

    /// Recoverable parser diagnostics observed but not persisted in the first schema.
    #[must_use]
    pub const fn diagnostics(&self) -> u64 {
        self.diagnostics
    }

    /// Bounded paths retained as partial after recoverable extraction limits.
    #[must_use]
    pub fn degraded_files(&self) -> &[NativeDegradedFile] {
        &self.degraded_files
    }

    /// Additional degraded paths omitted after the bounded report limit.
    #[must_use]
    pub const fn degraded_files_truncated(&self) -> u64 {
        self.degraded_files_truncated
    }

    /// Conservative Rust-owned bytes retained by the canonical COPY payload.
    #[must_use]
    pub const fn modeled_generation_bytes(&self) -> u64 {
        self.modeled_generation_bytes
    }

    /// Conservative cumulative allocation charge observed while resolving facts.
    #[must_use]
    pub const fn resolve_high_water_bytes(&self) -> u64 {
        self.resolve_high_water_bytes
    }

    /// Conservative cumulative allocation charge observed while canonicalizing the payload.
    #[must_use]
    pub const fn validation_high_water_bytes(&self) -> u64 {
        self.validation_high_water_bytes
    }

    /// Per-file replacement accounting when a persistent SCIP overlay was present.
    #[must_use]
    pub const fn scip_overlay(&self) -> Option<ScipOverlayReport> {
        self.scip_overlay
    }

    /// Conservative stage reservation for decoded and reconciled SCIP facts.
    #[must_use]
    pub const fn overlay_high_water_bytes(&self) -> u64 {
        self.overlay_high_water_bytes
    }

    /// Path/content/extractor cache activity. Zeroed when no cache was configured.
    #[must_use]
    pub const fn parse_cache(&self) -> NativeParseCacheReport {
        self.parse_cache
    }

    /// Physical working-set strategy used for this build.
    #[must_use]
    pub const fn storage(&self) -> NativeGenerationStorage {
        self.storage
    }

    /// Fixed-size PostgreSQL spill accounting, absent for the memory path.
    #[must_use]
    pub const fn spill(&self) -> Option<NativeGenerationSpillReport> {
        self.spill
    }
}

/// Canonical database facts plus fixed-size build accounting.
pub struct NativeGeneration {
    facts: CanonicalGenerationFacts,
    report: NativePipelineReport,
}

impl fmt::Debug for NativeGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeGeneration")
            .field("report", &self.report)
            .field("files", &self.facts.files().len())
            .field("symbols", &self.facts.symbols().len())
            .field("edges", &self.facts.edges().len())
            .field("references", &self.facts.references().len())
            .field("numerical_sites", &self.facts.numerical_sites().len())
            .field("documents", &self.facts.documents().len())
            .finish()
    }
}

impl NativeGeneration {
    /// Borrow the canonical facts before moving them into generation preparation.
    #[must_use]
    pub const fn facts(&self) -> &CanonicalGenerationFacts {
        &self.facts
    }

    /// Fixed-size discovery, extraction, and resolution accounting.
    #[must_use]
    pub fn report(&self) -> NativePipelineReport {
        self.report.clone()
    }

    /// Split the ready-to-COPY facts from their build report.
    #[must_use]
    pub fn into_parts(self) -> (CanonicalGenerationFacts, NativePipelineReport) {
        (self.facts, self.report)
    }
}

/// PostgreSQL-canonicalized native generation plus fixed-size build accounting.
pub struct NativeSpilledGeneration {
    digest: NativeGenerationSpillDigestReport,
    report: NativePipelineReport,
}

impl fmt::Debug for NativeSpilledGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeSpilledGeneration")
            .field("report", &self.report)
            .field("digest", self.digest.digest())
            .field("counts", &self.digest.counts())
            .finish()
    }
}

impl NativeSpilledGeneration {
    /// Fixed-size discovery, extraction, resolution, and spill accounting.
    #[must_use]
    pub fn report(&self) -> NativePipelineReport {
        self.report.clone()
    }

    /// Split the exact streamed digest capability from its public build report.
    #[must_use]
    pub fn into_parts(self) -> (NativeGenerationSpillDigestReport, NativePipelineReport) {
        (self.digest, self.report)
    }
}

/// Native project ingestion failed without embedding paths, source, or credentials.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NativePipelineError {
    /// Blocking filesystem and parser work requires Tokio's multi-thread runtime.
    #[error("native pipeline requires a multi-thread Tokio runtime")]
    Runtime,
    /// A bounded stage rejected, cancelled, or failed work.
    #[error(transparent)]
    Stage(#[from] StageRunError),
    /// A bounded stage rejected work for one allowlisted actionable reason.
    #[error("native pipeline failed during {stage}/{reason}")]
    StageWithReason {
        /// Exact stage whose worker returned the classified failure.
        stage: PipelineStage,
        /// Stable reason with source, identifier, path, and driver text discarded.
        reason: PipelineFailureReason,
    },
    /// One file-local stage input failed with a bounded project-relative diagnostic.
    #[error("native pipeline failed during {stage}/{failure}")]
    StageWithFileFailure {
        /// Exact stage whose worker returned the classified failure.
        stage: PipelineStage,
        /// Validated relative input path plus one allowlisted reason.
        failure: PipelineFileFailure,
    },
    /// Canonical validation failed with only an optional allowlisted reason retained.
    #[error("native pipeline canonical validation failed")]
    Validation {
        /// Stable actionable reason, when the failure matched an allowlisted boundary.
        reason: Option<PipelineFailureReason>,
    },
    /// Generation-fenced PostgreSQL spill failed without retaining driver details.
    #[error("native pipeline database spill failed during {stage}")]
    Spill {
        /// Exact native stage whose bounded spill operation failed.
        stage: PipelineStage,
    },
    /// A supposedly single-output stage violated its internal contract.
    #[error("native pipeline {stage} stage output was incomplete")]
    Incomplete {
        /// Exact stage whose single-output contract failed.
        stage: PipelineStage,
    },
}

impl NativePipelineError {
    /// Best exact stage retained by the native failure boundary.
    #[must_use]
    pub const fn stage(&self) -> PipelineStage {
        match self {
            Self::Runtime => PipelineStage::Discover,
            Self::Stage(error) => error.stage(),
            Self::StageWithReason { stage, .. }
            | Self::StageWithFileFailure { stage, .. }
            | Self::Spill { stage }
            | Self::Incomplete { stage } => *stage,
            Self::Validation { .. } => PipelineStage::Reduce,
        }
    }

    /// Optional allowlisted actionable reason with source and driver text discarded.
    #[must_use]
    pub const fn reason(&self) -> Option<PipelineFailureReason> {
        match self {
            Self::StageWithReason { reason, .. } => Some(*reason),
            Self::StageWithFileFailure { failure, .. } => Some(failure.reason()),
            Self::Validation { reason } => *reason,
            Self::Stage(
                StageRunError::StageDeadline { .. }
                | StageRunError::Item {
                    kind: StageFailureKind::Deadline,
                    ..
                },
            ) => Some(PipelineFailureReason::DeadlineExceeded),
            Self::Runtime | Self::Stage(_) | Self::Spill { .. } | Self::Incomplete { .. } => None,
        }
    }

    /// Optional project-relative input diagnostic retained by a file-local stage.
    #[must_use]
    pub const fn file_failure(&self) -> Option<&PipelineFileFailure> {
        match self {
            Self::StageWithFileFailure { failure, .. } => Some(failure),
            Self::Runtime
            | Self::Stage(_)
            | Self::StageWithReason { .. }
            | Self::Validation { .. }
            | Self::Spill { .. }
            | Self::Incomplete { .. } => None,
        }
    }
}

/// Complete bounded input for a native generation build with optional overlays and cache reads.
pub struct NativeGenerationBuild {
    source_root: SourceRoot,
    config: NativePipelineConfig,
    scip_overlay: Option<ScipOverlayInput>,
    parse_cache: Option<NativeParseCache>,
}

impl NativeGenerationBuild {
    /// Start a complete native build without optional SCIP or cache inputs.
    #[must_use]
    pub const fn new(source_root: SourceRoot, config: NativePipelineConfig) -> Self {
        Self {
            source_root,
            config,
            scip_overlay: None,
            parse_cache: None,
        }
    }

    /// Reconcile one validated SCIP overlay before canonical reduction.
    #[must_use]
    pub fn with_scip_overlay(mut self, overlay: ScipOverlayInput) -> Self {
        self.scip_overlay = Some(overlay);
        self
    }

    /// Reuse exact extraction facts through the bounded PostgreSQL cache.
    #[must_use]
    pub fn with_parse_cache(mut self, cache: NativeParseCache) -> Self {
        self.parse_cache = Some(cache);
        self
    }
}

/// Discover, hash, parse, resolve, and canonically reduce native source facts.
///
/// Source buffers are never retained across stages. Read/hash emits a compact manifest, parse
/// reopens each file under its exact observed size and rejects content drift, and ordered parse
/// output is moved into a separately bounded project fact accumulator before the worker
/// reservation is acknowledged.
/// # Errors
///
/// Returns an error if runtime/config validation, discovery, bounded source
/// reading, extraction, resolution, validation, or stage cleanup fails.
pub async fn build_native_generation(
    runner: &StageRunner,
    source_root: SourceRoot,
    config: NativePipelineConfig,
) -> Result<NativeGeneration, NativePipelineError> {
    build_native_generation_with_scip_and_cache(
        runner,
        NativeGenerationBuild::new(source_root, config),
    )
    .await
}

/// Build native facts and reconcile an optional persistent SCIP overlay before reduction.
/// # Errors
///
/// Returns an error if native construction fails or the optional SCIP overlay
/// is invalid, inconsistent with source facts, cancelled, or oversized.
pub async fn build_native_generation_with_scip(
    runner: &StageRunner,
    request: NativeGenerationBuild,
) -> Result<NativeGeneration, NativePipelineError> {
    build_native_generation_with_scip_and_cache(runner, request).await
}

/// Build native facts with an optional PostgreSQL path/content extraction cache.
///
/// Cache hits replace only the per-file parser invocation. Every build still assembles all files,
/// reruns project-wide resolution, validates the complete canonical generation, and publishes it
/// atomically, so incremental speed never weakens graph completeness.
/// # Errors
///
/// Returns an error if runtime/config validation or any discovery, cache,
/// read, parse, SCIP, resolve, memory-model, validation, or cleanup stage fails.
pub async fn build_native_generation_with_scip_and_cache(
    runner: &StageRunner,
    request: NativeGenerationBuild,
) -> Result<NativeGeneration, NativePipelineError> {
    let NativeGenerationBuild {
        source_root,
        config,
        scip_overlay,
        parse_cache,
    } = request;
    let parse_cache =
        parse_cache.map(|cache| cache.with_maximum_ast_depth(config.maximum_ast_depth()));
    require_multithread_runtime()?;
    let stages = NativeStageContext {
        runner,
        source_root,
        config,
    };
    let (discovered, exclusions) = run_discovery_stage(&stages).await?;
    let (manifest, skipped_oversized_files) = run_read_stage(&stages, discovered).await?;
    let report_seed = NativePipelineReport {
        discovered_files: usize_to_u64(manifest.entries.len()),
        excluded_paths: exclusions.excluded_paths(),
        excluded_trees: exclusions.excluded_trees(),
        skipped_oversized_files,
        source_bytes: manifest.source_bytes,
        ..NativePipelineReport::default()
    };
    let (mut extracted, parse_cache) =
        run_parse_stage(&stages, manifest.entries, parse_cache).await?;
    let degraded_files = take(&mut extracted.degraded_files);
    let degraded_files_truncated = extracted.degraded_files_truncated;
    let (facts, resolution) = run_resolve_stage(&stages, extracted).await?;
    let (facts, scip_overlay, overlay_high_water_bytes, reduction_progress_bytes) =
        match scip_overlay {
            Some(overlay) => {
                let output = run_scip_overlay_stage(&stages, facts, overlay).await?;
                (
                    output.facts,
                    Some(output.report),
                    output.high_water_bytes,
                    output.retained_bytes,
                )
            }
            None => (facts, None, 0, resolution.retained_bytes),
        };
    let (facts, validation) = run_reduce_stage(&stages, facts, reduction_progress_bytes).await?;
    let modeled_generation_bytes = validation.output_bytes();
    Ok(NativeGeneration {
        report: NativePipelineReport {
            symbols: usize_to_u64(facts.symbols().len()),
            numerical_sites: usize_to_u64(facts.numerical_sites().len()),
            resolved_references: resolution.resolved,
            unresolved_references: resolution.unresolved,
            diagnostics: resolution.diagnostics,
            degraded_files,
            degraded_files_truncated,
            modeled_generation_bytes,
            resolve_high_water_bytes: resolution.charged_high_water_bytes,
            validation_high_water_bytes: validation.charged_high_water_bytes(),
            scip_overlay,
            overlay_high_water_bytes,
            parse_cache,
            ..report_seed
        },
        facts,
    })
}

/// Build a native generation through generation-fenced PostgreSQL spill and reduction.
///
/// File-local extraction payloads and resolved relation batches are durable and retry-safe.
/// PostgreSQL performs the generation-wide canonical reduction in deterministic partitions,
/// after which the exact logical digest is streamed from canonical rows. The staging generation
/// remains invisible and this capability cannot publish it.
/// # Errors
///
/// Returns an error if the runtime, source, parser, resolver, spill quota/fence, canonical
/// reduction, source-verified overlay, or digest contract fails.
pub async fn build_native_generation_spilled(
    runner: &StageRunner,
    request: NativeGenerationBuild,
    spill: NativeGenerationSpill,
) -> Result<NativeSpilledGeneration, NativePipelineError> {
    let NativeGenerationBuild {
        source_root,
        config,
        scip_overlay,
        parse_cache,
    } = request;
    let parse_cache =
        parse_cache.map(|cache| cache.with_maximum_ast_depth(config.maximum_ast_depth()));
    require_multithread_runtime()?;
    let stages = NativeStageContext {
        runner,
        source_root,
        config,
    };
    let (discovered, exclusions) = run_discovery_stage(&stages).await?;
    let (manifest, skipped_oversized_files) = run_read_stage(&stages, discovered).await?;
    let report_seed = NativePipelineReport {
        discovered_files: usize_to_u64(manifest.entries.len()),
        excluded_paths: exclusions.excluded_paths(),
        excluded_trees: exclusions.excluded_trees(),
        skipped_oversized_files,
        source_bytes: manifest.source_bytes,
        storage: NativeGenerationStorage::PostgreSql,
        ..NativePipelineReport::default()
    };
    let (extracted, parse_cache) = run_spilled_parse_stage(
        &stages,
        SpilledParseStageInputs {
            manifest: manifest.entries,
            parse_cache,
            spill,
        },
    )
    .await?;
    let resolved = run_spilled_resolve_stage(&stages, extracted, scip_overlay).await?;
    let spill = resolved.spill;
    let digest = run_spilled_reduce_stage(&stages, spill.clone()).await?;
    let spill_report = spill
        .report()
        .await
        .map_err(|error| spill_pipeline_error(PipelineStage::Reduce, &error))?;
    let counts = digest.counts();
    Ok(NativeSpilledGeneration {
        report: NativePipelineReport {
            symbols: counts.symbols,
            numerical_sites: counts.numerical_sites,
            resolved_references: resolved.report.resolved,
            unresolved_references: resolved.report.unresolved,
            diagnostics: resolved.report.diagnostics,
            degraded_files: resolved.degraded_files,
            degraded_files_truncated: resolved.degraded_files_truncated,
            resolve_high_water_bytes: resolved.report.charged_high_water_bytes,
            validation_high_water_bytes: SPILLED_REDUCTION_RESERVATION_BYTES,
            parse_cache,
            scip_overlay: resolved.scip_overlay,
            overlay_high_water_bytes: resolved.overlay_high_water_bytes,
            spill: Some(spill_report),
            ..report_seed
        },
        digest,
    })
}

struct NativeStageContext<'a> {
    runner: &'a StageRunner,
    source_root: SourceRoot,
    config: NativePipelineConfig,
}

async fn run_discovery_stage(
    stages: &NativeStageContext<'_>,
) -> Result<(Vec<DiscoveredSource>, SourceExclusionEvidence), NativePipelineError> {
    let config = stages.config;
    let reservation = config
        .limits
        .discovery_limits
        .max_retained_path_bytes()
        .checked_add(usize_to_u64(size_of::<SourceRoot>()))
        .ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Discover,
        })?;
    let deadline = config.stage_deadlines();
    let item_deadline = planned_item_deadline(config.deadlines.item_timeout, deadline.deadline());
    let inputs = [StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(reservation.max(1), 0, item_deadline),
        ),
        stages.source_root.clone(),
    )];
    let limits = config.limits.discovery_limits;
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Discover, StageCapacity::new(1, 0), deadline),
        StageWorkload::new(
            inputs,
            move |item: StageWorkItem<u8, SourceRoot>| async move {
                let cancellation = item.cancellation();
                let (_, _, source_root) = item.into_parts();
                block_in_place(move || {
                    source_root
                        .discover_with_evidence(SourceDiscoveryOptions::new(limits, || {
                            cancellation.is_cancelled()
                        }))
                        .map_err(|_| StageItemFailure)
                })
            },
        ),
        StageFold::new(
            (Vec::new(), SourceExclusionEvidence::default()),
            |discovered: &mut (Vec<DiscoveredSource>, SourceExclusionEvidence),
             output: StageOutput<u8, (Vec<DiscoveredSource>, SourceExclusionEvidence)>| {
                let (_, output) = output.into_parts();
                *discovered = output;
                Ok(())
            },
        ),
    );
    stages.runner.execute(execution).await.map_err(Into::into)
}

async fn run_read_stage(
    stages: &NativeStageContext<'_>,
    discovered: Vec<DiscoveredSource>,
) -> Result<(SourceManifest, u64), NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let maximum_source_bytes = usize_to_u64(config.limits.source_limits.max_source_bytes());
    let skipped_oversized_files = usize_to_u64(
        discovered
            .iter()
            .filter(|source| source.byte_size() > maximum_source_bytes)
            .count(),
    );
    let envelopes = ReadEnvelopeIterator {
        sources: discovered
            .into_iter()
            .filter(move |source| source.byte_size() <= maximum_source_bytes),
        sequence: 0,
        item_timeout: config.deadlines.item_timeout,
        stage_deadline: deadline.deadline(),
        maximum_source_bytes,
    };
    let global_limits = config.limits.source_limits;
    let source_root = stages.source_root.clone();
    let execution = StageExecution::new(
        StageRunConfig::new(
            PipelineStage::Read,
            config.parallelism.read_capacity,
            deadline,
        ),
        StageWorkload::new(
            envelopes,
            move |item: StageWorkItem<NormalizedPath, DiscoveredSource>| {
                let source_root = source_root.clone();
                async move {
                    let cancellation = item.cancellation();
                    let (_, _, discovered) = item.into_parts();
                    block_in_place(move || {
                        read_manifest_entry(
                            ReadManifestInput {
                                source_root,
                                discovered,
                                global_limits,
                            },
                            || cancellation.is_cancelled(),
                        )
                    })
                }
            },
        ),
        StageFold::new(
            SourceManifest::new(config.limits.retained.max_manifest_bytes),
            |manifest: &mut SourceManifest,
             output: StageOutput<NormalizedPath, Option<SourceManifestEntry>>| {
                let (_, entry) = output.into_parts();
                match entry {
                    Some(entry) => manifest.push(entry),
                    None => Ok(()),
                }
            },
        ),
    );
    stages
        .runner
        .execute(execution)
        .await
        .map(|manifest| (manifest, skipped_oversized_files))
        .map_err(Into::into)
}

async fn run_parse_stage(
    stages: &NativeStageContext<'_>,
    manifest: Vec<SourceManifestEntry>,
    parse_cache: Option<NativeParseCache>,
) -> Result<(NativeFactAccumulator, NativeParseCacheReport), NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let envelopes = ParseEnvelopeIterator {
        entries: manifest.into_iter(),
        sequence: 0,
        item_timeout: config.deadlines.item_timeout,
        stage_deadline: deadline.deadline(),
    };
    let source_root = stages.source_root.clone();
    let worker_cache = parse_cache;
    let maximum_ast_depth = config.maximum_ast_depth();
    let failures = Arc::new(Mutex::new(BTreeMap::new()));
    let worker_failures = Arc::clone(&failures);
    let execution = StageExecution::new(
        StageRunConfig::new(
            PipelineStage::Parse,
            config.parallelism.parse_capacity,
            deadline,
        ),
        StageWorkload::new(
            envelopes,
            move |item: StageWorkItem<FileId, SourceManifestEntry>| {
                let source_root = source_root.clone();
                let parse_cache = worker_cache.clone();
                let failures = Arc::clone(&worker_failures);
                async move {
                    let cancellation = item.cancellation();
                    let (sequence, _, manifest) = item.into_parts();
                    let parsed = parse_manifest_entry_with_cache(
                        ParseManifestRequest {
                            source_root: &source_root,
                            manifest,
                            parse_cache: parse_cache.as_ref(),
                            maximum_ast_depth,
                        },
                        cancellation,
                    )
                    .await;
                    match parsed {
                        Ok(parsed) => Ok(parsed),
                        Err(failure) => {
                            if let Ok(mut retained) = failures.lock() {
                                retained.insert(sequence, failure);
                            }
                            Err(StageItemFailure)
                        }
                    }
                }
            },
        ),
        StageFold::new(
            ParseStageAccumulator::new(config.limits.retained.max_generation_bytes),
            |state: &mut ParseStageAccumulator,
             output: StageOutput<FileId, ParsedManifestEntry>| {
                let (_, parsed) = output.into_parts();
                state.cache.add(parsed.cache);
                state.facts.push(parsed.file)
            },
        ),
    );
    match stages.runner.execute(execution).await {
        Ok(state) => Ok((state.facts, state.cache)),
        Err(StageRunError::Reduce {
            stage: PipelineStage::Parse,
            ..
        }) => Err(NativePipelineError::StageWithReason {
            stage: PipelineStage::Parse,
            reason: PipelineFailureReason::GenerationCapacityExceeded,
        }),
        Err(error) => {
            let failure = match &error {
                StageRunError::Item {
                    stage: PipelineStage::Parse,
                    sequence,
                    kind: StageFailureKind::Worker,
                } => failures
                    .lock()
                    .ok()
                    .and_then(|retained| retained.get(sequence).cloned()),
                _ => None,
            };
            Err(classify_parse_stage_failure(error, failure))
        }
    }
}

fn classify_parse_stage_failure(
    error: StageRunError,
    failure: Option<ParseManifestFailure>,
) -> NativePipelineError {
    if failure.as_ref().is_some_and(ParseManifestFailure::is_spill) {
        return NativePipelineError::Spill {
            stage: PipelineStage::Parse,
        };
    }
    if let Some(file_failure) = failure
        .as_ref()
        .and_then(ParseManifestFailure::file_failure)
    {
        return NativePipelineError::StageWithFileFailure {
            stage: PipelineStage::Parse,
            failure: file_failure,
        };
    }
    match failure.and_then(|failure| failure.reason()) {
        Some(reason) => NativePipelineError::StageWithReason {
            stage: PipelineStage::Parse,
            reason,
        },
        None => NativePipelineError::Stage(error),
    }
}

#[derive(Clone)]
struct SpilledNativeFacts {
    spill: NativeGenerationSpill,
    state: NativeGenerationSpillState,
    files: u64,
    diagnostics: u64,
    degraded_files: Vec<NativeDegradedFile>,
    degraded_files_truncated: u64,
}

struct SpilledParseStageAccumulator {
    files: u64,
    diagnostics: u64,
    degraded_files: Vec<NativeDegradedFile>,
    degraded_files_truncated: u64,
    cache: NativeParseCacheReport,
}

impl SpilledParseStageAccumulator {
    const fn new() -> Self {
        Self {
            files: 0,
            diagnostics: 0,
            degraded_files: Vec::new(),
            degraded_files_truncated: 0,
            cache: NativeParseCacheReport {
                hits: 0,
                misses: 0,
                bypassed: 0,
                parsed_files: 0,
                writes: 0,
                corruptions: 0,
                read_errors: 0,
                write_errors: 0,
            },
        }
    }

    fn push(&mut self, mut parsed: SpilledParsedManifestBatch) -> Result<(), StageItemFailure> {
        self.files = self
            .files
            .checked_add(parsed.files)
            .ok_or(StageItemFailure)?;
        self.diagnostics = self
            .diagnostics
            .checked_add(parsed.diagnostics)
            .ok_or(StageItemFailure)?;
        self.degraded_files_truncated = self
            .degraded_files_truncated
            .checked_add(parsed.degraded_files_truncated)
            .ok_or(StageItemFailure)?;
        append_degraded_files(
            &mut self.degraded_files,
            &mut self.degraded_files_truncated,
            &mut parsed.degraded_files,
        )?;
        self.cache.add(parsed.cache);
        Ok(())
    }
}

struct SpilledParseManifestEntry {
    sequence: u64,
    manifest: SourceManifestEntry,
}

struct SpilledParseManifestBatch {
    entries: Vec<SpilledParseManifestEntry>,
}

struct SpilledParsedManifestBatch {
    files: u64,
    diagnostics: u64,
    degraded_files: Vec<NativeDegradedFile>,
    degraded_files_truncated: u64,
    cache: NativeParseCacheReport,
}

struct SpilledParseEnvelopeIterator {
    entries: Peekable<std::vec::IntoIter<SourceManifestEntry>>,
    file_sequence: u64,
    batch_sequence: u64,
    item_timeout: Duration,
    stage_deadline: Instant,
    failed: Arc<AtomicBool>,
}

impl SpilledParseEnvelopeIterator {
    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
    }
}

impl Iterator for SpilledParseEnvelopeIterator {
    type Item = StageEnvelope<u64, SpilledParseManifestBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.peek()?;
        let mut batch = Vec::new();
        if batch.try_reserve_exact(SPILLED_PARSE_BATCH_FILES).is_err() {
            self.fail();
            return None;
        }
        let mut maximum_reservation = 0_u64;
        let mut source_bytes = 0_u64;
        while batch.len() < SPILLED_PARSE_BATCH_FILES {
            let Some(next) = self.entries.peek() else {
                break;
            };
            let Some(next_source_bytes) = source_bytes.checked_add(next.byte_size) else {
                self.fail();
                return None;
            };
            if !batch.is_empty() && next_source_bytes > SPILLED_PARSE_BATCH_BYTES {
                break;
            }
            let Some(reservation) = native_extraction_reservation(next.byte_size) else {
                self.fail();
                return None;
            };
            let Some(manifest) = self.entries.next() else {
                self.fail();
                return None;
            };
            let sequence = self.file_sequence;
            let Some(next_file_sequence) = self.file_sequence.checked_add(1) else {
                self.fail();
                return None;
            };
            self.file_sequence = next_file_sequence;
            maximum_reservation = maximum_reservation.max(reservation);
            source_bytes = next_source_bytes;
            batch.push(SpilledParseManifestEntry { sequence, manifest });
            if source_bytes > SPILLED_PARSE_BATCH_BYTES {
                break;
            }
        }
        let Some(first) = batch.first() else {
            self.fail();
            return None;
        };
        let Some(reserved_bytes) =
            maximum_reservation.checked_add(SPILLED_PARSE_BATCH_RESERVATION_BYTES)
        else {
            self.fail();
            return None;
        };
        let sequence = self.batch_sequence;
        let Some(next_batch_sequence) = self.batch_sequence.checked_add(1) else {
            self.fail();
            return None;
        };
        self.batch_sequence = next_batch_sequence;
        let item_deadline = planned_item_deadline(self.item_timeout, self.stage_deadline);
        Some(StageEnvelope::new(
            StageItemMeta::new(
                StageSequence::new(sequence),
                first.sequence,
                StageItemBudget::new(reserved_bytes, source_bytes, item_deadline),
            ),
            SpilledParseManifestBatch { entries: batch },
        ))
    }
}

fn spilled_parse_envelopes(
    manifest: Vec<SourceManifestEntry>,
    config: NativePipelineConfig,
    stage_deadline: Instant,
) -> (SpilledParseEnvelopeIterator, Arc<AtomicBool>) {
    let failed = Arc::new(AtomicBool::new(false));
    (
        SpilledParseEnvelopeIterator {
            entries: manifest.into_iter().peekable(),
            file_sequence: 0,
            batch_sequence: 0,
            item_timeout: config.deadlines.item_timeout,
            stage_deadline,
            failed: Arc::clone(&failed),
        },
        failed,
    )
}

/// Manifest, optional parse cache, and spill authority for one parse stage.
struct SpilledParseStageInputs {
    manifest: Vec<SourceManifestEntry>,
    parse_cache: Option<NativeParseCache>,
    spill: NativeGenerationSpill,
}

async fn run_spilled_parse_stage(
    stages: &NativeStageContext<'_>,
    inputs: SpilledParseStageInputs,
) -> Result<(SpilledNativeFacts, NativeParseCacheReport), NativePipelineError> {
    let SpilledParseStageInputs {
        manifest,
        parse_cache,
        spill,
    } = inputs;
    let expected_files = usize_to_u64(manifest.len());
    let initial = spill
        .initialize()
        .await
        .map_err(|error| spill_pipeline_error(PipelineStage::Parse, &error))?;
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let (envelopes, envelope_failure) =
        spilled_parse_envelopes(manifest, config, deadline.deadline());
    let source_root = stages.source_root.clone();
    let worker_cache = parse_cache;
    let worker_spill = spill.clone();
    let maximum_ast_depth = config.maximum_ast_depth();
    let failures = Arc::new(Mutex::new(BTreeMap::new()));
    let worker_failures = Arc::clone(&failures);
    let execution = StageExecution::new(
        StageRunConfig::new(
            PipelineStage::Parse,
            config.parallelism.parse_capacity,
            deadline,
        ),
        StageWorkload::new(
            envelopes,
            move |item: StageWorkItem<u64, SpilledParseManifestBatch>| {
                let source_root = source_root.clone();
                let parse_cache = worker_cache.clone();
                let spill = worker_spill.clone();
                let failures = Arc::clone(&worker_failures);
                async move {
                    run_spilled_parse_item(
                        SpilledParseInputs {
                            spill: &spill,
                            source_root: &source_root,
                            parse_cache: parse_cache.as_ref(),
                            maximum_ast_depth,
                        },
                        item,
                        &failures,
                    )
                    .await
                }
            },
        ),
        StageFold::new(
            SpilledParseStageAccumulator::new(),
            |state: &mut SpilledParseStageAccumulator,
             output: StageOutput<u64, SpilledParsedManifestBatch>| {
                let (_, parsed) = output.into_parts();
                state.push(parsed)
            },
        ),
    );
    let result = stages.runner.execute(execution).await;
    if envelope_failure.load(Ordering::Acquire) {
        return Err(NativePipelineError::StageWithReason {
            stage: PipelineStage::Parse,
            reason: PipelineFailureReason::GenerationCapacityExceeded,
        });
    }
    let state = result.map_err(|error| classify_spilled_parse_stage_error(error, &failures))?;
    finish_spilled_parse_stage(
        spill,
        SpilledParseCompletion {
            initial_state: initial.state,
            expected_files,
            state,
        },
    )
    .await
}

/// Retain a parse item's detailed failure before returning the runner's worker error.
async fn run_spilled_parse_item(
    inputs: SpilledParseInputs<'_>,
    item: StageWorkItem<u64, SpilledParseManifestBatch>,
    failures: &Mutex<BTreeMap<StageSequence, ParseManifestFailure>>,
) -> Result<SpilledParsedManifestBatch, StageItemFailure> {
    let cancellation = item.cancellation();
    let (sequence, _, batch) = item.into_parts();
    match stream_spilled_parse_batch(inputs, batch, cancellation).await {
        Ok(parsed) => Ok(parsed),
        Err(failure) => {
            if let Ok(mut retained) = failures.lock() {
                retained.insert(sequence, failure);
            }
            Err(StageItemFailure)
        }
    }
}

struct SpilledParseCompletion {
    initial_state: NativeGenerationSpillState,
    expected_files: u64,
    state: SpilledParseStageAccumulator,
}

async fn finish_spilled_parse_stage(
    spill: NativeGenerationSpill,
    completion: SpilledParseCompletion,
) -> Result<(SpilledNativeFacts, NativeParseCacheReport), NativePipelineError> {
    let SpilledParseCompletion {
        initial_state,
        expected_files,
        state,
    } = completion;
    if state.files != expected_files {
        return Err(NativePipelineError::Incomplete {
            stage: PipelineStage::Parse,
        });
    }
    spill
        .finish_parsing(expected_files)
        .await
        .map_err(|error| spill_pipeline_error(PipelineStage::Parse, &error))?;
    let spill_state = match initial_state {
        NativeGenerationSpillState::Parsing => NativeGenerationSpillState::Resolving,
        state => state,
    };
    Ok((
        SpilledNativeFacts {
            spill,
            state: spill_state,
            files: state.files,
            diagnostics: state.diagnostics,
            degraded_files: state.degraded_files,
            degraded_files_truncated: state.degraded_files_truncated,
        },
        state.cache,
    ))
}

fn classify_spilled_parse_stage_error(
    error: StageRunError,
    failures: &Mutex<BTreeMap<StageSequence, ParseManifestFailure>>,
) -> NativePipelineError {
    let failure = match &error {
        StageRunError::Item {
            stage: PipelineStage::Parse,
            sequence,
            kind: StageFailureKind::Worker,
        } => failures
            .lock()
            .ok()
            .and_then(|retained| retained.get(sequence).cloned()),
        _ => None,
    };
    classify_parse_stage_failure(error, failure)
}

struct SpilledParseTransaction<'spill> {
    spill: &'spill NativeGenerationSpill,
    start_sequence: Option<u64>,
    rows: Vec<NativeGenerationSpillExtractedRow>,
    logical_bytes: u64,
}

impl<'spill> SpilledParseTransaction<'spill> {
    fn new(spill: &'spill NativeGenerationSpill) -> Result<Self, ParseManifestFailure> {
        let mut rows = Vec::new();
        rows.try_reserve_exact(SPILLED_PARSE_BATCH_FILES)
            .map_err(|_| ParseManifestFailure::unclassified())?;
        Ok(Self {
            spill,
            start_sequence: None,
            rows,
            logical_bytes: 0,
        })
    }

    async fn push_inline(
        &mut self,
        sequence: u64,
        file_id: &FileId,
        payload: Vec<u8>,
    ) -> Result<(), ParseManifestFailure> {
        let sort_key = file_id.as_str().as_bytes().to_vec();
        let row_bytes = usize_to_u64(sort_key.len()).saturating_add(usize_to_u64(payload.len()));
        self.prepare_push(sequence, row_bytes).await?;
        self.rows.push(
            NativeGenerationSpillRow::new(sort_key, payload)
                .map_err(|error| ParseManifestFailure::from_spill(&error))?
                .into(),
        );
        self.finish_push(row_bytes).await
    }

    async fn push_cached(
        &mut self,
        sequence: u64,
        file_id: &FileId,
        payload: CachedParsePayload,
    ) -> Result<(), ParseManifestFailure> {
        let CachedParsePayload {
            key,
            payload_digest,
            payload_bytes,
        } = payload;
        let sort_key = file_id.as_str().as_bytes().to_vec();
        let row_bytes = usize_to_u64(sort_key.len()).saturating_add(payload_bytes);
        self.prepare_push(sequence, row_bytes).await?;
        self.rows.push(
            NativeGenerationSpillCachedRow::new(
                sort_key,
                CachedRowPayload {
                    key,
                    payload_digest,
                    payload_bytes,
                },
            )
            .map_err(|error| ParseManifestFailure::from_spill(&error))?
            .into(),
        );
        self.finish_push(row_bytes).await
    }

    async fn prepare_push(
        &mut self,
        sequence: u64,
        row_bytes: u64,
    ) -> Result<(), ParseManifestFailure> {
        if self.pending_rows() > 0
            && (self.pending_rows() == SPILLED_PARSE_BATCH_FILES
                || self.logical_bytes.saturating_add(row_bytes) > SPILLED_PARSE_BATCH_BYTES)
        {
            self.flush().await?;
        }
        let start = self.start_sequence.get_or_insert(sequence);
        if start.saturating_add(usize_to_u64(self.pending_rows())) != sequence {
            return Err(ParseManifestFailure::unclassified());
        }
        Ok(())
    }

    async fn finish_push(&mut self, row_bytes: u64) -> Result<(), ParseManifestFailure> {
        self.logical_bytes = self.logical_bytes.saturating_add(row_bytes);
        if self.pending_rows() == SPILLED_PARSE_BATCH_FILES
            || self.logical_bytes >= SPILLED_PARSE_BATCH_BYTES
        {
            self.flush().await?;
        }
        Ok(())
    }

    fn pending_rows(&self) -> usize {
        self.rows.len()
    }

    async fn finish(mut self) -> Result<(), ParseManifestFailure> {
        self.flush().await
    }

    async fn flush(&mut self) -> Result<(), ParseManifestFailure> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let sequence = self
            .start_sequence
            .take()
            .ok_or_else(ParseManifestFailure::unclassified)?;
        self.logical_bytes = 0;
        let rows = take(&mut self.rows);
        self.rows
            .try_reserve_exact(SPILLED_PARSE_BATCH_FILES)
            .map_err(|_| ParseManifestFailure::unclassified())?;
        let batch = NativeGenerationSpillExtractedBatch::new(sequence, rows)
            .map_err(|error| ParseManifestFailure::from_spill(&error))?;
        self.spill
            .append_extracted_batch(batch)
            .await
            .map_err(|error| ParseManifestFailure::from_spill(&error))?;
        Ok(())
    }
}

struct PendingSpilledParseCacheWrite {
    sequence: u64,
    file_id: FileId,
    key: NativeParseCacheKey,
    payload: Vec<u8>,
    payload_digest: ContentDigest,
}

impl PendingSpilledParseCacheWrite {
    fn new(write: ParsedFilePayload, key: NativeParseCacheKey) -> Self {
        let ParsedFilePayload {
            sequence,
            file_id,
            payload,
        } = write;
        let payload_digest = ContentDigest::from_bytes(*blake3::hash(&payload).as_bytes());
        Self {
            sequence,
            file_id,
            key,
            payload,
            payload_digest,
        }
    }
}

struct SpilledParseBatchContext<'input, 'spill> {
    source_root: &'input SourceRoot,
    parse_cache: Option<&'input NativeParseCache>,
    cancellation: &'input StageCancellation,
    extractors: SpilledExtractorPool,
    transaction: SpilledParseTransaction<'spill>,
    output: SpilledParsedManifestBatch,
    cache_writes: Vec<PendingSpilledParseCacheWrite>,
    cache_write_bytes: usize,
}

impl<'input, 'spill> SpilledParseBatchContext<'input, 'spill> {
    fn new(
        spill: &'spill NativeGenerationSpill,
        batch: SpilledParseBatchInputs<'input>,
        capacity: usize,
    ) -> Result<Self, ParseManifestFailure> {
        let SpilledParseBatchInputs {
            inputs,
            cancellation,
        } = batch;
        let SpilledParseInputs {
            source_root,
            parse_cache,
            maximum_ast_depth,
            ..
        } = inputs;
        let mut cache_writes = Vec::new();
        cache_writes
            .try_reserve_exact(capacity)
            .map_err(|_| ParseManifestFailure::unclassified())?;
        Ok(Self {
            source_root,
            parse_cache,
            cancellation,
            extractors: SpilledExtractorPool::new(maximum_ast_depth),
            transaction: SpilledParseTransaction::new(spill)?,
            output: SpilledParsedManifestBatch {
                files: 0,
                diagnostics: 0,
                degraded_files: Vec::new(),
                degraded_files_truncated: 0,
                cache: NativeParseCacheReport::default(),
            },
            cache_writes,
            cache_write_bytes: 0,
        })
    }

    async fn process(
        &mut self,
        entry: SpilledParseManifestEntry,
        lookup: CachedParseLookup<'_>,
    ) -> Result<(), ParseManifestFailure> {
        let CachedParseLookup {
            key,
            cached,
            cache_read_failed,
        } = lookup;
        let mut metrics = NativeParseCacheReport::default();
        if let Some((file, payload_digest, payload_bytes)) = self
            .cached_file(
                &entry.manifest,
                CachedParseLookup {
                    key,
                    cached,
                    cache_read_failed,
                },
                &mut metrics,
            )
            .await?
        {
            record_spilled_parse_output(&mut self.output, &file, metrics)?;
            self.flush_cache_writes().await?;
            let Some(key) = key else {
                return Err(ParseManifestFailure::unclassified());
            };
            self.transaction
                .push_cached(
                    entry.sequence,
                    &file.file_id,
                    CachedParsePayload {
                        key: key.clone(),
                        payload_digest,
                        payload_bytes,
                    },
                )
                .await?;
            return Ok(());
        }
        if self.cancellation.is_cancelled() {
            return Err(ParseManifestFailure::unclassified());
        }
        metrics.parsed_files = 1;
        let parse_cancellation = self.cancellation.clone();
        let source_root = self.source_root;
        let manifest = entry.manifest;
        let extractors = &mut self.extractors;
        let file = block_in_place(move || {
            extractors.extract(source_root, &manifest, || parse_cancellation.is_cancelled())
        })?;
        if self.cancellation.is_cancelled() {
            return Err(ParseManifestFailure::unclassified());
        }
        let payload =
            serde_json::to_vec(&file).map_err(|_| ParseManifestFailure::unclassified())?;
        let cacheable = self.parse_cache.is_some()
            && key.is_some()
            && payload.len() <= MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES;
        if self.parse_cache.is_some() && !cacheable {
            metrics.write_errors = 1;
        }
        record_spilled_parse_output(&mut self.output, &file, metrics)?;
        if cacheable {
            return self
                .queue_cache_write(
                    ParsedFilePayload {
                        sequence: entry.sequence,
                        file_id: file.file_id,
                        payload,
                    },
                    key,
                )
                .await;
        }
        self.flush_cache_writes().await?;
        self.transaction
            .push_inline(entry.sequence, &file.file_id, payload)
            .await
    }

    async fn cached_file(
        &self,
        manifest: &SourceManifestEntry,
        lookup: CachedParseLookup<'_>,
        metrics: &mut NativeParseCacheReport,
    ) -> Result<Option<(ExtractedFile, ContentDigest, u64)>, ParseManifestFailure> {
        let CachedParseLookup {
            key,
            cached,
            cache_read_failed,
        } = lookup;
        let (Some(cache), Some(key)) = (self.parse_cache, key) else {
            return Ok(None);
        };
        if !cache.read_enabled {
            metrics.bypassed = 1;
        } else if cache_read_failed {
            metrics.misses = 1;
            metrics.read_errors = 1;
        } else if let Some(record) = cached {
            if let Some(file) = decode_cached_file(manifest, &record) {
                revalidate_manifest(self.source_root, manifest, || {
                    self.cancellation.is_cancelled()
                })?;
                metrics.hits = 1;
                return Ok(Some((
                    file,
                    record.payload_digest().clone(),
                    usize_to_u64(record.payload().len()),
                )));
            }
            metrics.misses = 1;
            metrics.corruptions = 1;
            let _ = cache.database.evict_native_parse_cache(key).await;
        } else {
            metrics.misses = 1;
        }
        Ok(None)
    }

    async fn queue_cache_write(
        &mut self,
        write: ParsedFilePayload,
        key: Option<&NativeParseCacheKey>,
    ) -> Result<(), ParseManifestFailure> {
        let ParsedFilePayload {
            sequence,
            file_id,
            payload,
        } = write;
        let Some(key) = key else {
            return Err(ParseManifestFailure::unclassified());
        };
        if !self.cache_writes.is_empty()
            && self.cache_write_bytes.saturating_add(payload.len())
                > SPILLED_PARSE_CACHE_WRITE_BYTES
        {
            self.flush_cache_writes().await?;
        }
        self.cache_write_bytes = self.cache_write_bytes.saturating_add(payload.len());
        self.cache_writes.push(PendingSpilledParseCacheWrite::new(
            ParsedFilePayload {
                sequence,
                file_id,
                payload,
            },
            key.clone(),
        ));
        if self.cache_write_bytes >= SPILLED_PARSE_CACHE_WRITE_BYTES {
            self.flush_cache_writes().await?;
        }
        Ok(())
    }

    async fn flush_cache_writes(&mut self) -> Result<(), ParseManifestFailure> {
        if self.cache_writes.is_empty() {
            return Ok(());
        }
        let Some(cache) = self.parse_cache else {
            return Err(ParseManifestFailure::unclassified());
        };
        flush_spilled_parse_cache_writes(
            cache,
            &mut PendingCacheWrites {
                writes: &mut self.cache_writes,
                logical_bytes: &mut self.cache_write_bytes,
                report: &mut self.output.cache,
            },
            &mut self.transaction,
        )
        .await
    }

    async fn finish(mut self) -> Result<SpilledParsedManifestBatch, ParseManifestFailure> {
        self.flush_cache_writes().await?;
        self.transaction.finish().await?;
        Ok(self.output)
    }
}

/// Spill authority, source root, and optional parse cache for one batch.
#[derive(Clone, Copy)]
struct SpilledParseInputs<'inputs> {
    spill: &'inputs NativeGenerationSpill,
    source_root: &'inputs SourceRoot,
    parse_cache: Option<&'inputs NativeParseCache>,
    maximum_ast_depth: usize,
}

async fn stream_spilled_parse_batch(
    inputs: SpilledParseInputs<'_>,
    batch: SpilledParseManifestBatch,
    cancellation: StageCancellation,
) -> Result<SpilledParsedManifestBatch, ParseManifestFailure> {
    let SpilledParseInputs {
        spill,
        source_root,
        parse_cache,
        maximum_ast_depth,
    } = inputs;
    let entries = batch.entries;
    let (cache_keys, cached, cache_read_failed) =
        load_spilled_parse_cache(parse_cache, &entries).await;
    let mut context = SpilledParseBatchContext::new(
        spill,
        SpilledParseBatchInputs {
            inputs: SpilledParseInputs {
                spill,
                source_root,
                parse_cache,
                maximum_ast_depth,
            },
            cancellation: &cancellation,
        },
        entries.len(),
    )?;
    for (index, (entry, cached)) in entries.into_iter().zip(cached).enumerate() {
        let path = entry.manifest.path.clone();
        let key = cache_keys.as_ref().and_then(|keys| keys.get(index));
        context
            .process(
                entry,
                CachedParseLookup {
                    key,
                    cached,
                    cache_read_failed,
                },
            )
            .await
            .map_err(|failure| failure.with_path(path))?;
    }
    context.finish().await
}

async fn load_spilled_parse_cache(
    cache: Option<&NativeParseCache>,
    entries: &[SpilledParseManifestEntry],
) -> (
    Option<Vec<NativeParseCacheKey>>,
    Vec<Option<NativeParseCacheRecord>>,
    bool,
) {
    let keys = cache.map(|cache| {
        entries
            .iter()
            .map(|entry| cache.key(&entry.manifest))
            .collect::<Vec<_>>()
    });
    match (cache, keys.as_ref()) {
        (Some(cache), Some(keys)) if cache.read_enabled => {
            match cache.database.load_native_parse_cache_batch(keys).await {
                Ok(records) => (Some(keys.clone()), records, false),
                Err(_) => (
                    Some(keys.clone()),
                    (0..entries.len()).map(|_| None).collect(),
                    true,
                ),
            }
        }
        _ => (keys, (0..entries.len()).map(|_| None).collect(), false),
    }
}

fn record_spilled_parse_output(
    output: &mut SpilledParsedManifestBatch,
    file: &ExtractedFile,
    metrics: NativeParseCacheReport,
) -> Result<(), ParseManifestFailure> {
    output.files = output
        .files
        .checked_add(1)
        .ok_or_else(ParseManifestFailure::unclassified)?;
    output.diagnostics = output
        .diagnostics
        .checked_add(usize_to_u64(file.diagnostics.len()))
        .ok_or_else(ParseManifestFailure::unclassified)?;
    record_degraded_file(
        &mut output.degraded_files,
        &mut output.degraded_files_truncated,
        file,
    )
    .map_err(|_| ParseManifestFailure::unclassified())?;
    output.cache.add(metrics);
    Ok(())
}

/// Pending cache writes and the accounting they advance.
struct PendingCacheWrites<'writes> {
    writes: &'writes mut Vec<PendingSpilledParseCacheWrite>,
    logical_bytes: &'writes mut usize,
    report: &'writes mut NativeParseCacheReport,
}

async fn flush_spilled_parse_cache_writes(
    cache: &NativeParseCache,
    pending: &mut PendingCacheWrites<'_>,
    transaction: &mut SpilledParseTransaction<'_>,
) -> Result<(), ParseManifestFailure> {
    let PendingCacheWrites {
        writes,
        logical_bytes,
        report,
    } = pending;
    if writes.is_empty() {
        return Ok(());
    }
    let pending = take(*writes);
    let pending_rows = usize_to_u64(pending.len());
    **logical_bytes = 0;
    let cache_entries = pending
        .iter()
        .map(|write| NativeParseCacheEntry::new(write.key.clone(), write.payload.clone()))
        .collect();
    if let Ok(written) = cache
        .database
        .store_native_parse_cache_batch(cache_entries)
        .await
    {
        report.writes = report.writes.saturating_add(written.inserted);
        for write in pending {
            transaction
                .push_cached(
                    write.sequence,
                    &write.file_id,
                    CachedParsePayload {
                        key: write.key,
                        payload_digest: write.payload_digest,
                        payload_bytes: usize_to_u64(write.payload.len()),
                    },
                )
                .await?;
        }
    } else {
        report.write_errors = report.write_errors.saturating_add(pending_rows);
        for write in pending {
            transaction
                .push_inline(write.sequence, &write.file_id, write.payload)
                .await?;
        }
    }
    Ok(())
}

async fn visit_spilled_native_files<State, Visit>(
    walk: SpilledFileWalk<'_>,
    state: &mut State,
    mut visit: Visit,
) -> Result<(), StageItemFailure>
where
    Visit: FnMut(&mut State, NativeFileFacts) -> Result<(), StageItemFailure>,
{
    let SpilledFileWalk {
        source,
        observation:
            SpilledVisitObservation {
                cancellation,
                progress,
            },
    } = walk;
    let mut cursor = NativeGenerationExtractedCursor::default();
    let mut expected_sequence = 0_u64;
    loop {
        if cancellation.is_cancelled() {
            return Err(StageItemFailure);
        }
        let page = source
            .spill
            .load_extracted_page(cursor, SPILLED_EXTRACTION_PAGE_BYTES)
            .await
            .map_err(|_| StageItemFailure)?;
        if page.is_empty() {
            break;
        }
        let completed_items = usize_to_u64(page.rows().len());
        let completed_bytes = page.rows().iter().try_fold(0_u64, |total, (_, row)| {
            total.checked_add(usize_to_u64(row.payload().len()))
        });
        let completed_bytes = completed_bytes.ok_or(StageItemFailure)?;
        for (sequence, row) in page.rows() {
            if *sequence != expected_sequence || cancellation.is_cancelled() {
                return Err(StageItemFailure);
            }
            let extracted = serde_json::from_slice::<ExtractedFile>(row.payload())
                .map_err(|_| StageItemFailure)?;
            let file = NativeFileFacts::from_extracted(extracted)?;
            visit(state, file)?;
            expected_sequence = expected_sequence.checked_add(1).ok_or(StageItemFailure)?;
        }
        progress
            .advance_progress(completed_items, completed_bytes)
            .await
            .map_err(|_| StageItemFailure)?;
        cursor = page.next();
    }
    (expected_sequence == source.files).ok_or(StageItemFailure)
}

struct ParseStageAccumulator {
    facts: NativeFactAccumulator,
    cache: NativeParseCacheReport,
}

impl ParseStageAccumulator {
    const fn new(maximum_bytes: u64) -> Self {
        Self {
            facts: NativeFactAccumulator::new(maximum_bytes),
            cache: NativeParseCacheReport {
                hits: 0,
                misses: 0,
                bypassed: 0,
                parsed_files: 0,
                writes: 0,
                corruptions: 0,
                read_errors: 0,
                write_errors: 0,
            },
        }
    }
}

struct ParsedManifestEntry {
    file: ExtractedFile,
    cache: NativeParseCacheReport,
}

struct ParseManifestRequest<'input> {
    source_root: &'input SourceRoot,
    manifest: SourceManifestEntry,
    parse_cache: Option<&'input NativeParseCache>,
    maximum_ast_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParseManifestFailure {
    reason: Option<PipelineFailureReason>,
    path: Option<NormalizedPath>,
    scope: ParseManifestFailureScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParseManifestFailureScope {
    File,
    Stage,
    Spill,
}

impl ParseManifestFailure {
    const fn unclassified() -> Self {
        Self {
            reason: None,
            path: None,
            scope: ParseManifestFailureScope::File,
        }
    }

    const fn from_extract(error: ExtractError) -> Self {
        let reason = match error {
            ExtractError::UnsupportedLanguage => {
                Some(PipelineFailureReason::ExtractionUnsupportedLanguage)
            }
            ExtractError::LanguageMismatch => {
                Some(PipelineFailureReason::ExtractionLanguageMismatch)
            }
            ExtractError::GrammarUnavailable => {
                Some(PipelineFailureReason::ExtractionGrammarUnavailable)
            }
            ExtractError::ParserStopped => Some(PipelineFailureReason::ExtractionParserStopped),
            ExtractError::Cancelled => Some(PipelineFailureReason::ExtractionCancelled),
            ExtractError::InvalidSpan => Some(PipelineFailureReason::ExtractionInvalidSpan),
            ExtractError::NestingLimit => {
                Some(PipelineFailureReason::ExtractionNestingLimitExceeded)
            }
            ExtractError::InvalidNestingLimit => {
                Some(PipelineFailureReason::ExtractionInvalidNestingLimit)
            }
            ExtractError::OutputLimit => Some(PipelineFailureReason::ExtractionOutputLimitExceeded),
        };
        Self {
            reason,
            path: None,
            scope: ParseManifestFailureScope::File,
        }
    }

    const fn from_source_read(error: SourceReadError) -> Self {
        let reason = match error {
            SourceReadError::Cancelled => PipelineFailureReason::ExtractionCancelled,
            SourceReadError::RootUnavailable
            | SourceReadError::InvalidPolicy
            | SourceReadError::FileUnavailable
            | SourceReadError::OutsideRoot
            | SourceReadError::NotRegularFile
            | SourceReadError::SourceTooLarge
            | SourceReadError::ResourceLimit
            | SourceReadError::UnsupportedLanguage
            | SourceReadError::InvalidSnapshot => PipelineFailureReason::SourceReadFailed,
        };
        Self {
            reason: Some(reason),
            path: None,
            scope: ParseManifestFailureScope::File,
        }
    }

    fn from_spill(error: &cartograph_db::StorageError) -> Self {
        if spill_capacity_error(error) {
            Self {
                reason: Some(PipelineFailureReason::GenerationCapacityExceeded),
                path: None,
                scope: ParseManifestFailureScope::Stage,
            }
        } else {
            Self {
                reason: None,
                path: None,
                scope: ParseManifestFailureScope::Spill,
            }
        }
    }

    const fn source_changed() -> Self {
        Self {
            reason: Some(PipelineFailureReason::SourceChangedDuringParse),
            path: None,
            scope: ParseManifestFailureScope::File,
        }
    }

    fn with_path(mut self, path: NormalizedPath) -> Self {
        if self.scope == ParseManifestFailureScope::File {
            self.path = Some(path);
            if self.reason.is_none() {
                self.reason = Some(PipelineFailureReason::FileProcessingFailed);
            }
        }
        self
    }

    const fn reason(&self) -> Option<PipelineFailureReason> {
        self.reason
    }

    fn file_failure(&self) -> Option<PipelineFileFailure> {
        if self.scope != ParseManifestFailureScope::File {
            return None;
        }
        Some(PipelineFileFailure::new(self.path.clone()?, self.reason?))
    }

    const fn is_spill(&self) -> bool {
        matches!(self.scope, ParseManifestFailureScope::Spill)
    }
}

fn spill_pipeline_error(
    stage: PipelineStage,
    error: &cartograph_db::StorageError,
) -> NativePipelineError {
    match spill_failure_reason(error) {
        Some(reason) => NativePipelineError::StageWithReason { stage, reason },
        None => NativePipelineError::Spill { stage },
    }
}

/// Actionable reason for a spill failure. A spill statement that outlives its
/// bounded statement timeout is that stage's deadline, not an opaque failure.
fn spill_failure_reason(error: &cartograph_db::StorageError) -> Option<PipelineFailureReason> {
    if spill_capacity_error(error) {
        Some(PipelineFailureReason::GenerationCapacityExceeded)
    } else if matches!(error, cartograph_db::StorageError::StatementTimeout { .. }) {
        Some(PipelineFailureReason::DeadlineExceeded)
    } else {
        None
    }
}

fn spill_capacity_error(error: &cartograph_db::StorageError) -> bool {
    match error {
        cartograph_db::StorageError::GenerationSpillLimitReached { .. } => true,
        cartograph_db::StorageError::InvalidInput { field } => matches!(
            *field,
            "spill_payload"
                | "spill_batch_rows"
                | "spill_batch_bytes"
                | "spill_fact_batch_rows"
                | "spill_fact_batch_bytes"
        ),
        _ => false,
    }
}

async fn parse_manifest_entry_with_cache(
    request: ParseManifestRequest<'_>,
    cancellation: StageCancellation,
) -> Result<ParsedManifestEntry, ParseManifestFailure> {
    let ParseManifestRequest {
        source_root,
        manifest,
        parse_cache,
        maximum_ast_depth,
    } = request;
    let path = manifest.path.clone();
    let mut metrics = NativeParseCacheReport::default();
    let key = parse_cache.map(|cache| cache.key(&manifest));
    if let (Some(cache), Some(key)) = (parse_cache, key.as_ref()) {
        if cache.read_enabled {
            if let Some(file) = load_cached_manifest(CacheReadRequest {
                cache,
                key,
                source_root,
                manifest: &manifest,
                cancellation: &cancellation,
                metrics: &mut metrics,
            })
            .await
            .map_err(|failure| failure.with_path(path.clone()))?
            {
                return Ok(ParsedManifestEntry {
                    file,
                    cache: metrics,
                });
            }
        } else {
            metrics.bypassed = 1;
        }
    }
    if cancellation.is_cancelled() {
        return Err(
            ParseManifestFailure::from_extract(ExtractError::Cancelled).with_path(path.clone())
        );
    }
    metrics.parsed_files = 1;
    let parse_cancellation = cancellation.clone();
    let file = block_in_place(move || {
        let request = ParseSourceRequest {
            source_root,
            manifest,
            maximum_ast_depth,
        };
        parse_manifest_entry(&request, || parse_cancellation.is_cancelled())
    })?;
    if cancellation.is_cancelled() {
        return Err(
            ParseManifestFailure::from_extract(ExtractError::Cancelled).with_path(path.clone())
        );
    }
    if let (Some(cache), Some(key)) = (parse_cache, key.as_ref()) {
        store_cached_manifest(CacheWriteRequest {
            cache,
            key,
            file: &file,
            metrics: &mut metrics,
        })
        .await
        .map_err(|failure| failure.with_path(path))?;
    }
    Ok(ParsedManifestEntry {
        file,
        cache: metrics,
    })
}

struct CacheReadRequest<'a> {
    cache: &'a NativeParseCache,
    key: &'a NativeParseCacheKey,
    source_root: &'a SourceRoot,
    manifest: &'a SourceManifestEntry,
    cancellation: &'a StageCancellation,
    metrics: &'a mut NativeParseCacheReport,
}

async fn load_cached_manifest(
    request: CacheReadRequest<'_>,
) -> Result<Option<ExtractedFile>, ParseManifestFailure> {
    let CacheReadRequest {
        cache,
        key,
        source_root,
        manifest,
        cancellation,
        metrics,
    } = request;
    match cache.database.load_native_parse_cache(key).await {
        Ok(Some(record)) => {
            let Some(file) = decode_cached_file(manifest, &record) else {
                metrics.misses = 1;
                metrics.corruptions = 1;
                let _ = cache.database.evict_native_parse_cache(key).await;
                return Ok(None);
            };
            revalidate_manifest(source_root, manifest, || cancellation.is_cancelled())?;
            metrics.hits = 1;
            Ok(Some(file))
        }
        Ok(None) => {
            metrics.misses = 1;
            Ok(None)
        }
        Err(_) => {
            metrics.misses = 1;
            metrics.read_errors = 1;
            let _ = cache.database.evict_native_parse_cache(key).await;
            Ok(None)
        }
    }
}

struct CacheWriteRequest<'a> {
    cache: &'a NativeParseCache,
    key: &'a NativeParseCacheKey,
    file: &'a ExtractedFile,
    metrics: &'a mut NativeParseCacheReport,
}

async fn store_cached_manifest(request: CacheWriteRequest<'_>) -> Result<(), ParseManifestFailure> {
    let CacheWriteRequest {
        cache,
        key,
        file,
        metrics,
    } = request;
    let payload = serde_json::to_vec(file).map_err(|_| ParseManifestFailure::unclassified())?;
    if payload.len() > MAX_NATIVE_PARSE_CACHE_PAYLOAD_BYTES {
        metrics.write_errors = 1;
        return Ok(());
    }
    match cache.database.store_native_parse_cache(key, &payload).await {
        Ok(NativeParseCacheWrite::Inserted) => metrics.writes = 1,
        Ok(NativeParseCacheWrite::AlreadyPresent) => {}
        Err(_) => metrics.write_errors = 1,
    }
    Ok(())
}

fn decode_cached_file(
    manifest: &SourceManifestEntry,
    record: &cartograph_db::NativeParseCacheRecord,
) -> Option<ExtractedFile> {
    let payload_digest = ContentDigest::from_bytes(*blake3::hash(record.payload()).as_bytes());
    if record.payload_digest() != &payload_digest || record.source_bytes() != manifest.byte_size {
        return None;
    }
    let file = serde_json::from_slice::<ExtractedFile>(record.payload()).ok()?;
    (file.file_id == manifest.file_id
        && file.path == manifest.path
        && file.language == manifest.language
        && file.content_hash == manifest.content_hash
        && file.byte_size == manifest.byte_size)
        .then_some(file)
}

fn revalidate_manifest<Cancel>(
    source_root: &SourceRoot,
    manifest: &SourceManifestEntry,
    mut cancelled: Cancel,
) -> Result<(), ParseManifestFailure>
where
    Cancel: FnMut() -> bool,
{
    block_in_place(|| read_manifest_snapshot(source_root, manifest, &mut cancelled)).map(|_| ())
}

async fn run_resolve_stage(
    stages: &NativeStageContext<'_>,
    extracted: NativeFactAccumulator,
) -> Result<(GenerationFacts, ResolutionReport), NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let item_deadline = deadline.deadline();
    let inputs = [StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(
                resolve_reservation(config.limits.retained.max_generation_bytes)?,
                extracted.retained_bytes,
                item_deadline,
            ),
        ),
        extracted,
    )];
    let maximum = config.limits.retained.max_generation_bytes;
    let evidence_policy = config.evidence_policy();
    let clone_policy = config.clone_policy();
    let source_root = stages.source_root.clone();
    let failure_reason = Arc::new(Mutex::new(None));
    let worker_failure_reason = Arc::clone(&failure_reason);
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Resolve, StageCapacity::new(1, 0), deadline),
        StageWorkload::new(
            inputs,
            move |item: StageWorkItem<u8, NativeFactAccumulator>| {
                let source_root = source_root.clone();
                let worker_failure_reason = Arc::clone(&worker_failure_reason);
                async move {
                    let cancellation = item.cancellation();
                    let (_, _, extracted) = item.into_parts();
                    let result = block_in_place(move || {
                        resolve_generation(
                            ResolveGenerationRequest {
                                extracted,
                                maximum_bytes: maximum,
                                source_root,
                                evidence_policy,
                                clone_policy,
                            },
                            || cancellation.is_cancelled(),
                        )
                    });
                    match result {
                        Ok(output) => Ok(output),
                        Err(failure) => {
                            if let Some(reason) = failure.reason()
                                && let Ok(mut retained) = worker_failure_reason.lock()
                            {
                                *retained = Some(reason);
                            }
                            Err(StageItemFailure)
                        }
                    }
                }
            },
        ),
        StageFold::new(
            None,
            |resolved: &mut Option<(GenerationFacts, ResolutionReport)>,
             output: StageOutput<u8, (GenerationFacts, ResolutionReport)>| {
                let (_, output) = output.into_parts();
                *resolved = Some(output);
                Ok(())
            },
        ),
    );
    match stages.runner.execute(execution).await {
        Ok(output) => output.ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Resolve,
        }),
        Err(error @ StageRunError::Item { .. }) => {
            let reason = failure_reason.lock().ok().and_then(|retained| *retained);
            match reason {
                Some(reason) => Err(NativePipelineError::StageWithReason {
                    stage: PipelineStage::Resolve,
                    reason,
                }),
                None => Err(NativePipelineError::Stage(error)),
            }
        }
        Err(error) => Err(NativePipelineError::Stage(error)),
    }
}

struct SpilledResolutionOutput {
    scip_overlay: Option<ScipOverlayReport>,
    overlay_high_water_bytes: u64,
    spill: NativeGenerationSpill,
    report: ResolutionReport,
    degraded_files: Vec<NativeDegradedFile>,
    degraded_files_truncated: u64,
}

struct SpilledResolutionState {
    overlay: Option<ScipOverlayPlan>,
    clone_evidence: CloneEvidenceMap,
    index: Arc<ResolutionIndex>,
    report: ResolutionReport,
    counts: NativeGenerationSpillFactCounts,
    high_water: u64,
    centrality_enabled: bool,
    centrality: GenerationFacts,
    centrality_budget: ResolveBudget,
    validation_limits: GenerationValidationLimits,
}

struct ResolvedFileFacts {
    sequence: u64,
    facts: GenerationFacts,
    report: ResolutionReport,
    high_water: u64,
}

struct SpilledFactTransaction<'spill> {
    spill: &'spill NativeGenerationSpill,
    progress: &'spill StageRunner,
    batches: Vec<NativeGenerationSpillFactBatch>,
    logical_bytes: u64,
    row_count: u64,
    retained_bytes: u64,
}

impl<'spill> SpilledFactTransaction<'spill> {
    fn new(spill: &'spill NativeGenerationSpill, progress: &'spill StageRunner) -> Self {
        Self {
            spill,
            progress,
            batches: Vec::new(),
            logical_bytes: 0,
            row_count: 0,
            retained_bytes: 0,
        }
    }

    async fn push(
        &mut self,
        batch: NativeGenerationSpillFactBatch,
    ) -> Result<(), ResolveGenerationFailure> {
        let next_bytes = self
            .logical_bytes
            .checked_add(batch.logical_bytes())
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        let next_rows = self
            .row_count
            .checked_add(batch.row_count())
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        let next_retained = self
            .retained_bytes
            .checked_add(batch.retained_bytes())
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        if !self.batches.is_empty()
            && (self.batches.len() >= SPILLED_RESOLVE_TRANSACTION_BATCHES
                || next_bytes > SPILLED_RESOLVE_TRANSACTION_BYTES
                || next_rows > SPILLED_RESOLVE_TRANSACTION_ROWS
                || next_retained > SPILLED_RESOLVE_TRANSACTION_RETAINED_BYTES)
        {
            self.flush().await?;
        }
        self.logical_bytes = self
            .logical_bytes
            .checked_add(batch.logical_bytes())
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        self.row_count = self
            .row_count
            .checked_add(batch.row_count())
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(batch.retained_bytes())
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        self.batches
            .try_reserve(1)
            .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
        self.batches.push(batch);
        if self.batches.len() >= SPILLED_RESOLVE_TRANSACTION_BATCHES {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), ResolveGenerationFailure> {
        if self.batches.is_empty() {
            return Ok(());
        }
        let batches = take(&mut self.batches);
        let expected = batches.len();
        let completed_items = usize_to_u64(expected);
        let completed_bytes = self.logical_bytes;
        self.logical_bytes = 0;
        self.row_count = 0;
        self.retained_bytes = 0;
        let writes = self
            .spill
            .append_fact_batches(batches)
            .await
            .map_err(|error| classify_spill_resolve_error(&error))?;
        if writes.len() != expected {
            return Err(ResolveGenerationFailure::unclassified());
        }
        self.progress
            .advance_progress(completed_items, completed_bytes)
            .await
            .map_err(|_| ResolveGenerationFailure::unclassified())?;
        Ok(())
    }
}

struct SpilledResolutionFold<'context> {
    state: &'context mut SpilledResolutionState,
    transaction: SpilledFactTransaction<'context>,
    completed: BTreeMap<u64, ResolvedFileFacts>,
    next_sequence: u64,
    cancellation: &'context StageCancellation,
}

impl<'context> SpilledResolutionFold<'context> {
    fn new(
        spill: &'context NativeGenerationSpill,
        state: &'context mut SpilledResolutionState,
        observation: SpilledVisitObservation<'context>,
    ) -> Self {
        let SpilledVisitObservation {
            cancellation,
            progress,
        } = observation;
        Self {
            state,
            transaction: SpilledFactTransaction::new(spill, progress),
            completed: BTreeMap::new(),
            next_sequence: 0,
            cancellation,
        }
    }

    async fn collect_next(
        &mut self,
        tasks: &mut JoinSet<Result<ResolvedFileFacts, ResolveGenerationFailure>>,
    ) -> Result<(), ResolveGenerationFailure> {
        let resolved = tasks
            .join_next()
            .await
            .ok_or_else(ResolveGenerationFailure::unclassified)?
            .map_err(|_| ResolveGenerationFailure::unclassified())??;
        if self.completed.insert(resolved.sequence, resolved).is_some() {
            return Err(ResolveGenerationFailure::unclassified());
        }
        while let Some(resolved) = self.completed.remove(&self.next_sequence) {
            self.commit_resolved(resolved).await?;
        }
        Ok(())
    }

    async fn commit_resolved(
        &mut self,
        mut resolved: ResolvedFileFacts,
    ) -> Result<(), ResolveGenerationFailure> {
        if resolved.sequence != self.next_sequence || self.cancellation.is_cancelled() {
            return Err(ResolveGenerationFailure::unclassified());
        }
        self.state.report.resolved = self
            .state
            .report
            .resolved
            .checked_add(resolved.report.resolved)
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        self.state.report.unresolved = self
            .state
            .report
            .unresolved
            .checked_add(resolved.report.unresolved)
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        self.state.high_water = self.state.high_water.max(resolved.high_water);
        scip_spill::filter_native(
            self.state.overlay.as_ref(),
            &mut resolved.facts,
            self.cancellation,
        )?;
        if self.state.centrality_enabled {
            append_spilled_centrality_facts(
                &mut self.state.centrality,
                &resolved.facts,
                &mut self.state.centrality_budget,
            )
            .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
        }
        let batch = NativeGenerationSpillFactBatch::new(
            FactBatchInput {
                sequence: resolved.sequence,
                facts: resolved.facts,
                limits: self.state.validation_limits,
            },
            || self.cancellation.is_cancelled(),
        )
        .map_err(classify_spill_validation_error)?;
        add_spill_fact_counts(&mut self.state.counts, batch.counts())?;
        self.transaction.push(batch).await?;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        Ok(())
    }

    async fn finish(mut self) -> Result<u64, ResolveGenerationFailure> {
        if !self.completed.is_empty() {
            return Err(ResolveGenerationFailure::unclassified());
        }
        self.transaction.flush().await?;
        Ok(self.next_sequence)
    }
}

async fn run_spilled_resolve_stage(
    stages: &NativeStageContext<'_>,
    source: SpilledNativeFacts,
    overlay: Option<ScipOverlayInput>,
) -> Result<SpilledResolutionOutput, NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let item_deadline = deadline.deadline();
    let inputs = [StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(
                resolve_reservation(config.limits.retained.max_generation_bytes)?,
                source.files,
                item_deadline,
            ),
        ),
        (source, overlay),
    )];
    let source_root = stages.source_root.clone();
    let progress = stages.runner.clone();
    let failure_reason = Arc::new(Mutex::new(None));
    let worker_failure_reason = Arc::clone(&failure_reason);
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Resolve, StageCapacity::new(1, 0), deadline),
        StageWorkload::new(
            inputs,
            move |item: StageWorkItem<u8, (SpilledNativeFacts, Option<ScipOverlayInput>)>| {
                let source_root = source_root.clone();
                let progress = progress.clone();
                let failure_reason = Arc::clone(&worker_failure_reason);
                async move {
                    let cancellation = item.cancellation();
                    let (_, _, (source, overlay)) = item.into_parts();
                    match resolve_spilled_generation(
                        SpilledGenerationSource {
                            facts: source,
                            root: source_root,
                            overlay,
                        },
                        SpilledStageContext {
                            config,
                            cancellation: &cancellation,
                            progress: &progress,
                        },
                    )
                    .await
                    {
                        Ok(output) => Ok(output),
                        Err(failure) => {
                            if let Some(reason) = failure.reason()
                                && let Ok(mut retained) = failure_reason.lock()
                            {
                                *retained = Some(reason);
                            }
                            Err(StageItemFailure)
                        }
                    }
                }
            },
        ),
        StageFold::new(
            None,
            |resolved: &mut Option<SpilledResolutionOutput>,
             output: StageOutput<u8, SpilledResolutionOutput>| {
                let (_, output) = output.into_parts();
                *resolved = Some(output);
                Ok(())
            },
        ),
    );
    match stages.runner.execute(execution).await {
        Ok(output) => output.ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Resolve,
        }),
        Err(error @ StageRunError::Item { .. }) => {
            let reason = failure_reason.lock().ok().and_then(|retained| *retained);
            match reason {
                Some(reason) => Err(NativePipelineError::StageWithReason {
                    stage: PipelineStage::Resolve,
                    reason,
                }),
                None => Err(NativePipelineError::Stage(error)),
            }
        }
        Err(error) => Err(NativePipelineError::Stage(error)),
    }
}

/// Observation channel for one spilled file walk.
#[derive(Clone, Copy)]
struct SpilledVisitObservation<'observation> {
    cancellation: &'observation StageCancellation,
    progress: &'observation StageRunner,
}

/// Language visibility for one resolution-candidate reservation.
#[derive(Clone, Copy)]
struct CandidateLanguageVisibility<'visibility> {
    language: &'visibility str,
    globally_visible: bool,
    new_language_bucket: bool,
}

/// The spilled facts one file walk reads, with its observation channel.
#[derive(Clone, Copy)]
struct SpilledFileWalk<'walk> {
    source: &'walk SpilledNativeFacts,
    observation: SpilledVisitObservation<'walk>,
}

/// Batch inputs paired with the cancellation they are observed under.
#[derive(Clone, Copy)]
struct SpilledParseBatchInputs<'inputs> {
    inputs: SpilledParseInputs<'inputs>,
    cancellation: &'inputs StageCancellation,
}

/// Resolution index and the budget its growth is charged against.
struct ResolutionIndexTarget<'target> {
    index: &'target mut ResolutionIndex,
    budget: &'target mut ResolveBudget,
}

/// One parsed file's sequence, identity, and encoded payload.
struct ParsedFilePayload {
    sequence: u64,
    file_id: FileId,
    payload: Vec<u8>,
}

/// Working bound and cancellation for one derived-fact pass.
#[derive(Clone, Copy)]
struct DerivedFactBound<'bound> {
    cancellation: &'bound StageCancellation,
    maximum_bytes: u64,
}

/// One clone-prefilter histogram with its total token count.
#[derive(Clone, Copy)]
struct ClonePrefilter<'prefilter> {
    buckets: &'prefilter [u32; CLONE_PREFILTER_BUCKETS],
    total: u32,
}

/// Cache key and payload accounting for one cached parse result.
struct CachedParsePayload {
    key: NativeParseCacheKey,
    payload_digest: ContentDigest,
    payload_bytes: u64,
}

/// Cache lookup outcome for one manifest entry.
struct CachedParseLookup<'lookup> {
    key: Option<&'lookup NativeParseCacheKey>,
    cached: Option<NativeParseCacheRecord>,
    cache_read_failed: bool,
}

/// Resolution index, configuration, and cancellation for one file.
#[derive(Clone, Copy)]
struct SpilledFileResolution<'resolution> {
    index: &'resolution ResolutionIndex,
    config: NativePipelineConfig,
    cancellation: &'resolution StageCancellation,
}

/// Clone policy, working bound, and observation channel for one preparation.
#[derive(Clone, Copy)]
struct ResolutionPreparationRequest<'request> {
    policy: NativeClonePolicy,
    maximum_bytes: u64,
    cancellation: &'request StageCancellation,
    progress: &'request StageRunner,
}

/// Fixed configuration, cancellation, and progress reporting for one spilled stage.
#[derive(Clone, Copy)]
struct SpilledStageContext<'context> {
    config: NativePipelineConfig,
    cancellation: &'context StageCancellation,
    progress: &'context StageRunner,
}

struct SpilledGenerationSource {
    facts: SpilledNativeFacts,
    root: SourceRoot,
    overlay: Option<ScipOverlayInput>,
}

async fn resolve_spilled_generation(
    input: SpilledGenerationSource,
    context: SpilledStageContext<'_>,
) -> Result<SpilledResolutionOutput, ResolveGenerationFailure> {
    let SpilledGenerationSource {
        facts: source,
        root: source_root,
        overlay,
    } = input;
    let SpilledStageContext {
        config: _,
        cancellation,
        progress,
    } = context;
    if cancellation.is_cancelled() {
        return Err(ResolveGenerationFailure::unclassified());
    }
    let (overlay, overlay_high_water_bytes, config) = context
        .prepare_overlay(&source, &source_root, overlay)
        .await?;
    let context = SpilledStageContext { config, ..context };
    let overlay_report = overlay.as_ref().map(ScipOverlayPlan::report);
    let mut state = initialize_spilled_resolution(&source, &source_root, context).await?;
    state.overlay = overlay;
    spill_resolved_files(&source, &mut state, context).await?;
    let next_sequence = spill_derived_generation_facts(&source, &mut state, context).await?;
    state
        .append_imported(&source, next_sequence, context)
        .await?;
    if state.centrality_enabled {
        apply_spilled_centrality(
            &source.spill,
            &mut state.centrality,
            SpilledCentralityRequest {
                policy: config.evidence_policy().centrality,
                cancellation,
                progress,
            },
        )
        .await?;
        state.high_water = state.high_water.max(state.centrality_budget.charged_bytes);
    }
    source
        .spill
        .seal_resolution(state.counts)
        .await
        .map_err(|error| classify_spill_resolve_error(&error))?;
    state.report.retained_bytes = state.high_water;
    state.report.charged_high_water_bytes = state.high_water;
    Ok(SpilledResolutionOutput {
        scip_overlay: overlay_report,
        overlay_high_water_bytes,
        spill: source.spill,
        report: state.report,
        degraded_files: source.degraded_files,
        degraded_files_truncated: source.degraded_files_truncated,
    })
}

async fn initialize_spilled_resolution(
    source: &SpilledNativeFacts,
    source_root: &SourceRoot,
    context: SpilledStageContext<'_>,
) -> Result<SpilledResolutionState, ResolveGenerationFailure> {
    let SpilledStageContext {
        config,
        cancellation,
        progress,
    } = context;
    let maximum_bytes = config.limits.retained.max_generation_bytes;
    let (clone_evidence, index, preparation_high_water) = build_spilled_resolution_preparation(
        source,
        source_root,
        ResolutionPreparationRequest {
            policy: config.clone_policy(),
            maximum_bytes,
            cancellation,
            progress,
        },
    )
    .await
    .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    let evidence = config.evidence_policy();
    let centrality_enabled = source.state == NativeGenerationSpillState::Resolving
        && (evidence.centrality.page_rank || evidence.centrality.betweenness);
    let centrality_limit = maximum_bytes
        .checked_mul(RESOLVE_WORKING_MULTIPLIER)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    let centrality_budget = ResolveBudget::new(0, centrality_limit)
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    let validation_limits = generation_validation_limits(maximum_bytes, PipelineStage::Resolve)
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    Ok(SpilledResolutionState {
        overlay: None,
        clone_evidence,
        index: Arc::new(index),
        report: ResolutionReport {
            diagnostics: source.diagnostics,
            ..ResolutionReport::default()
        },
        counts: NativeGenerationSpillFactCounts::default(),
        high_water: preparation_high_water,
        centrality_enabled,
        centrality: GenerationFacts::default(),
        centrality_budget,
        validation_limits,
    })
}

async fn spill_resolved_files(
    source: &SpilledNativeFacts,
    state: &mut SpilledResolutionState,
    context: SpilledStageContext<'_>,
) -> Result<(), ResolveGenerationFailure> {
    let SpilledStageContext {
        config,
        cancellation,
        progress,
    } = context;
    let mut cursor = NativeGenerationExtractedCursor::default();
    let mut input_sequence = 0_u64;
    let worker_count = config.parallelism.parse_capacity.workers();
    let mut tasks = JoinSet::new();
    let mut fold = SpilledResolutionFold::new(
        &source.spill,
        state,
        SpilledVisitObservation {
            cancellation,
            progress,
        },
    );
    loop {
        if cancellation.is_cancelled() {
            return Err(ResolveGenerationFailure::unclassified());
        }
        let page = source
            .spill
            .load_extracted_page(cursor, SPILLED_EXTRACTION_PAGE_BYTES)
            .await
            .map_err(|error| classify_spill_resolve_error(&error))?;
        if page.is_empty() {
            break;
        }
        schedule_spilled_resolution_page(
            &page,
            SpilledResolutionSchedule {
                worker_count,
                config,
                cancellation,
            },
            &mut SpilledResolutionProgress {
                input_sequence: &mut input_sequence,
                tasks: &mut tasks,
                fold: &mut fold,
            },
        )
        .await?;
        cursor = page.next();
    }
    while !tasks.is_empty() {
        fold.collect_next(&mut tasks).await?;
    }
    if input_sequence != source.files || !fold.state.clone_evidence.is_empty() {
        return Err(ResolveGenerationFailure::unclassified());
    }
    let resolved_files = fold.finish().await?;
    (resolved_files == source.files).ok_or(ResolveGenerationFailure::unclassified())
}

/// Fixed per-run scheduling policy for one spilled resolution pass.
#[derive(Clone, Copy)]
struct SpilledResolutionSchedule<'schedule> {
    worker_count: usize,
    config: NativePipelineConfig,
    cancellation: &'schedule StageCancellation,
}

/// Mutable scheduling state advanced across every loaded page.
struct SpilledResolutionProgress<'progress, 'fold> {
    input_sequence: &'progress mut u64,
    tasks: &'progress mut JoinSet<Result<ResolvedFileFacts, ResolveGenerationFailure>>,
    fold: &'progress mut SpilledResolutionFold<'fold>,
}

async fn schedule_spilled_resolution_page(
    page: &NativeGenerationExtractedPage,
    schedule: SpilledResolutionSchedule<'_>,
    progress: &mut SpilledResolutionProgress<'_, '_>,
) -> Result<(), ResolveGenerationFailure> {
    let SpilledResolutionSchedule {
        worker_count,
        config,
        cancellation,
    } = schedule;
    let SpilledResolutionProgress {
        input_sequence,
        tasks,
        fold,
    } = progress;
    for (sequence, row) in page.rows() {
        require_spilled_resolution_sequence(*sequence, **input_sequence, cancellation)?;
        let extracted = serde_json::from_slice::<ExtractedFile>(row.payload())
            .map_err(|_| ResolveGenerationFailure::unclassified())?;
        let mut file = NativeFileFacts::from_extracted(extracted)
            .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
        apply_clone_evidence(&mut file, &mut fold.state.clone_evidence)
            .map_err(|_| ResolveGenerationFailure::unclassified())?;
        let index = Arc::clone(&fold.state.index);
        let worker_cancellation = cancellation.clone();
        let sequence = *sequence;
        tasks.spawn_blocking(move || {
            resolve_file_facts(
                sequence,
                file,
                SpilledFileResolution {
                    index: index.as_ref(),
                    config,
                    cancellation: &worker_cancellation,
                },
            )
        });
        **input_sequence = input_sequence
            .checked_add(1)
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        if tasks.len() >= worker_count {
            fold.collect_next(tasks).await?;
        }
    }
    Ok(())
}

fn require_spilled_resolution_sequence(
    actual: u64,
    expected: u64,
    cancellation: &StageCancellation,
) -> Result<(), ResolveGenerationFailure> {
    if actual != expected || cancellation.is_cancelled() {
        Err(ResolveGenerationFailure::unclassified())
    } else {
        Ok(())
    }
}

fn resolve_file_facts(
    sequence: u64,
    file: NativeFileFacts,
    resolution: SpilledFileResolution<'_>,
) -> Result<ResolvedFileFacts, ResolveGenerationFailure> {
    let SpilledFileResolution {
        index,
        config,
        cancellation,
    } = resolution;
    let maximum_bytes = config.limits.retained.max_generation_bytes;
    let mut facts = GenerationFacts::default();
    let mut report = ResolutionReport::default();
    let working_limit = maximum_bytes
        .checked_mul(RESOLVE_WORKING_MULTIPLIER)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    let mut budget = ResolveBudget::new(0, working_limit)
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    {
        let mut output = ResolutionOutput {
            index,
            facts: &mut facts,
            report: &mut report,
            budget: &mut budget,
        };
        output
            .append_file(file, &mut || cancellation.is_cancelled())
            .map_err(|_| classify_resolve_failure(output.budget))?;
    }
    if !config.evidence_policy().retention.call_sites {
        facts.references.clear();
    }
    Ok(ResolvedFileFacts {
        sequence,
        facts,
        report,
        high_water: budget.charged_bytes,
    })
}

async fn spill_derived_generation_facts(
    source: &SpilledNativeFacts,
    state: &mut SpilledResolutionState,
    context: SpilledStageContext<'_>,
) -> Result<u64, ResolveGenerationFailure> {
    let SpilledStageContext {
        config,
        cancellation,
        progress,
    } = context;
    let maximum_bytes = config.limits.retained.max_generation_bytes;
    let mut derived_sequence = source.files;
    for kind in SpilledDerivedFactKind::ALL {
        // Deriving one kind walks the whole resolution index and validates the
        // batch synchronously; keep that work off the async worker core so it
        // cannot hold tasks scheduled on this worker for the whole walk.
        let batch = block_in_place(|| {
            derive_spilled_fact_batch(
                state,
                DerivedFactBatchRequest {
                    kind,
                    sequence: derived_sequence,
                    bound: DerivedFactBound {
                        cancellation,
                        maximum_bytes,
                    },
                },
            )
        })?;
        if let Some(batch) = batch {
            source
                .spill
                .append_fact_batch(batch)
                .await
                .map_err(|error| classify_spill_resolve_error(&error))?;
            derived_sequence = derived_sequence
                .checked_add(1)
                .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        }
        progress
            .advance_progress(1, 0)
            .await
            .map_err(|_| ResolveGenerationFailure::unclassified())?;
    }
    Ok(derived_sequence)
}

/// One derived fact kind and the batch sequence it would occupy.
#[derive(Clone, Copy)]
struct DerivedFactBatchRequest<'request> {
    kind: SpilledDerivedFactKind,
    sequence: u64,
    bound: DerivedFactBound<'request>,
}

/// Derive, overlay-filter, and validate one derived fact kind.
///
/// Returns `None` when the kind produced no facts. Counts and centrality input
/// are recorded before the batch is returned for durable append.
fn derive_spilled_fact_batch(
    state: &mut SpilledResolutionState,
    request: DerivedFactBatchRequest<'_>,
) -> Result<Option<NativeGenerationSpillFactBatch>, ResolveGenerationFailure> {
    let DerivedFactBatchRequest {
        kind,
        sequence,
        bound,
    } = request;
    let cancellation = bound.cancellation;
    let (mut facts, charged) = derive_spilled_facts(&state.index, bound, kind)
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    state.high_water = state.high_water.max(charged);
    scip_spill::filter_native(state.overlay.as_ref(), &mut facts, cancellation)?;
    if generation_facts_are_empty(&facts) {
        return Ok(None);
    }
    if state.centrality_enabled {
        append_spilled_centrality_facts(
            &mut state.centrality,
            &facts,
            &mut state.centrality_budget,
        )
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    }
    let batch = NativeGenerationSpillFactBatch::new(
        FactBatchInput {
            sequence,
            facts,
            limits: state.validation_limits,
        },
        || cancellation.is_cancelled(),
    )
    .map_err(classify_spill_validation_error)?;
    add_spill_fact_counts(&mut state.counts, batch.counts())?;
    Ok(Some(batch))
}

fn append_spilled_centrality_facts(
    centrality: &mut GenerationFacts,
    facts: &GenerationFacts,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    centrality
        .symbols
        .try_reserve(facts.symbols.len())
        .map_err(|_| StageItemFailure)?;
    for symbol in &facts.symbols {
        budget.charge(
            usize_to_u64(size_of::<SymbolInput>())
                .saturating_add(usize_to_u64(symbol.symbol_id.as_str().len()))
                .saturating_add(usize_to_u64(symbol.file_id.as_str().len())),
        )?;
        centrality.symbols.push(SymbolInput {
            symbol_id: symbol.symbol_id.clone(),
            file_id: symbol.file_id.clone(),
            symbol_kind: String::new(),
            qualified_name: String::new(),
            signature: String::new(),
            start_byte: 0,
            end_byte: 0,
            start_line: 1,
            end_line: 1,
            structural_digest: symbol.structural_digest.clone(),
            visibility: None,
            export: SymbolExportFlags::default(),
            execution: SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        });
    }
    let admitted_edges = facts
        .edges
        .iter()
        .filter(|edge| matches!(edge.kind, EdgeKind::Calls | EdgeKind::References))
        .count();
    centrality
        .edges
        .try_reserve(admitted_edges)
        .map_err(|_| StageItemFailure)?;
    for edge in facts
        .edges
        .iter()
        .filter(|edge| matches!(edge.kind, EdgeKind::Calls | EdgeKind::References))
    {
        budget.charge(
            usize_to_u64(size_of::<EdgeInput>())
                .saturating_add(usize_to_u64(edge.source_symbol_id.as_str().len()))
                .saturating_add(usize_to_u64(edge.target_symbol_id.as_str().len())),
        )?;
        centrality.edges.push(EdgeInput {
            source_symbol_id: edge.source_symbol_id.clone(),
            target_symbol_id: edge.target_symbol_id.clone(),
            kind: edge.kind,
            confidence: 0.0,
            provenance: String::new(),
            site_count: 1,
        });
    }
    Ok(())
}

/// Centrality policy plus the observation channel for one spilled pass.
#[derive(Clone, Copy)]
struct SpilledCentralityRequest<'request> {
    policy: NativeCentralityPolicy,
    cancellation: &'request StageCancellation,
    progress: &'request StageRunner,
}

async fn apply_spilled_centrality(
    spill: &NativeGenerationSpill,
    facts: &mut GenerationFacts,
    request: SpilledCentralityRequest<'_>,
) -> Result<(), ResolveGenerationFailure> {
    let SpilledCentralityRequest {
        policy,
        cancellation,
        progress,
    } = request;
    // PageRank, sampled betweenness, and score ordering are whole-graph CPU
    // work; run them off the async worker core.
    if policy.page_rank {
        let report = block_in_place(|| apply_page_rank(facts, || cancellation.is_cancelled()))
            .map_err(|_| ResolveGenerationFailure::unclassified())?;
        if report.iterations > 0 {
            progress
                .advance_progress(usize_to_u64(report.iterations), 0)
                .await
                .map_err(|_| ResolveGenerationFailure::unclassified())?;
        }
    }
    if policy.betweenness {
        let report =
            block_in_place(|| apply_sampled_betweenness(facts, || cancellation.is_cancelled()))
                .map_err(|_| ResolveGenerationFailure::unclassified())?;
        let completed = report.sample_count.max(report.nodes_scored);
        if completed > 0 {
            progress
                .advance_progress(usize_to_u64(completed), 0)
                .await
                .map_err(|_| ResolveGenerationFailure::unclassified())?;
        }
    }
    let scores = block_in_place(|| ordered_centrality_scores(facts))?;
    for batch in scores.chunks(SPILLED_CENTRALITY_UPDATE_ROWS) {
        if cancellation.is_cancelled() {
            return Err(ResolveGenerationFailure::unclassified());
        }
        spill
            .apply_centrality_scores(batch)
            .await
            .map_err(|error| classify_spill_resolve_error(&error))?;
        progress
            .advance_progress(usize_to_u64(batch.len()), 0)
            .await
            .map_err(|_| ResolveGenerationFailure::unclassified())?;
    }
    Ok(())
}

/// Centrality scores for every symbol, ordered by symbol identifier.
fn ordered_centrality_scores(
    facts: &GenerationFacts,
) -> Result<Vec<NativeGenerationSpillCentralityScore>, ResolveGenerationFailure> {
    let mut scores = Vec::new();
    scores
        .try_reserve(facts.symbols.len())
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    for symbol in &facts.symbols {
        scores.push(
            NativeGenerationSpillCentralityScore::new(
                symbol.symbol_id.clone(),
                symbol.betweenness_ppb,
                symbol.pagerank_ppb,
            )
            .map_err(|_| ResolveGenerationFailure::unclassified())?,
        );
    }
    scores
        .sort_unstable_by(|left, right| left.symbol_id().as_str().cmp(right.symbol_id().as_str()));
    Ok(scores)
}

#[derive(Clone, Copy)]
enum SpilledDerivedFactKind {
    Framework,
    Go,
    Reexport,
    Test,
}

impl SpilledDerivedFactKind {
    /// Derived fact kinds in the exact order the resolution stage appends them.
    const ALL: [Self; 4] = [Self::Framework, Self::Go, Self::Reexport, Self::Test];
}

fn derive_spilled_facts(
    index: &ResolutionIndex,
    bound: DerivedFactBound<'_>,
    kind: SpilledDerivedFactKind,
) -> Result<(GenerationFacts, u64), StageItemFailure> {
    let DerivedFactBound {
        cancellation,
        maximum_bytes,
    } = bound;
    let working_limit = maximum_bytes
        .checked_mul(RESOLVE_WORKING_MULTIPLIER)
        .ok_or(StageItemFailure)?;
    let mut facts = GenerationFacts::default();
    let mut budget = ResolveBudget::new(0, working_limit)?;
    let mut cancelled = || cancellation.is_cancelled();
    match kind {
        SpilledDerivedFactKind::Framework => {
            append_framework_bridge_edges(ResolutionMutation {
                index,
                facts: &mut facts,
                budget: &mut budget,
                cancelled: &mut cancelled,
            })?;
        }
        SpilledDerivedFactKind::Go => {
            append_go_structural_edges(GoStructuralEdges {
                index,
                facts: &mut facts,
                budget: &mut budget,
                cancelled: &mut cancelled,
            })?;
        }
        SpilledDerivedFactKind::Reexport => {
            append_module_reexport_edges(ResolutionMutation {
                index,
                facts: &mut facts,
                budget: &mut budget,
                cancelled: &mut cancelled,
            })?;
        }
        SpilledDerivedFactKind::Test => {
            append_test_subject_edges(ResolutionMutation {
                index,
                facts: &mut facts,
                budget: &mut budget,
                cancelled: &mut cancelled,
            })?;
        }
    }
    Ok((facts, budget.charged_bytes))
}

fn generation_facts_are_empty(facts: &GenerationFacts) -> bool {
    facts.files.is_empty()
        && facts.symbols.is_empty()
        && facts.edges.is_empty()
        && facts.references.is_empty()
        && facts.numerical_sites.is_empty()
        && facts.documents.is_empty()
}

fn add_spill_fact_counts(
    total: &mut NativeGenerationSpillFactCounts,
    addition: NativeGenerationSpillFactCounts,
) -> Result<(), ResolveGenerationFailure> {
    total.files = total
        .files
        .checked_add(addition.files)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    total.symbols = total
        .symbols
        .checked_add(addition.symbols)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    total.edges = total
        .edges
        .checked_add(addition.edges)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    total.references = total
        .references
        .checked_add(addition.references)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    total.numerical_sites = total
        .numerical_sites
        .checked_add(addition.numerical_sites)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    total.documents = total
        .documents
        .checked_add(addition.documents)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    Ok(())
}

fn classify_spill_resolve_error(error: &cartograph_db::StorageError) -> ResolveGenerationFailure {
    if spill_capacity_error(error) {
        ResolveGenerationFailure::generation_capacity_exceeded()
    } else {
        ResolveGenerationFailure::unclassified()
    }
}

fn classify_spill_validation_error(error: GenerationValidationError) -> ResolveGenerationFailure {
    match error {
        GenerationValidationError::RetainedLimit => {
            ResolveGenerationFailure::generation_capacity_exceeded()
        }
        GenerationValidationError::Storage(error) if spill_capacity_error(&error) => {
            ResolveGenerationFailure::generation_capacity_exceeded()
        }
        GenerationValidationError::ReferenceNameTooLong => ResolveGenerationFailure {
            reason: Some(PipelineFailureReason::ReferenceNameTooLong),
        },
        GenerationValidationError::Storage(StorageError::InvalidInput { field }) => {
            ResolveGenerationFailure {
                reason: Some(PipelineFailureReason::CanonicalFieldRejected(field)),
            }
        }
        GenerationValidationError::Storage(_) | GenerationValidationError::Cancelled => {
            ResolveGenerationFailure::unclassified()
        }
    }
}

struct ScipOverlayWork {
    facts: GenerationFacts,
    overlay: ScipOverlayInput,
}

struct ScipOverlayStageOutput {
    facts: GenerationFacts,
    report: ScipOverlayReport,
    retained_bytes: u64,
    high_water_bytes: u64,
}

async fn run_scip_overlay_stage(
    stages: &NativeStageContext<'_>,
    facts: GenerationFacts,
    overlay: ScipOverlayInput,
) -> Result<ScipOverlayStageOutput, NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let item_deadline = planned_item_deadline(config.deadlines.item_timeout, deadline.deadline());
    let high_water_bytes = generation_validation_limits(
        config.limits.retained.max_generation_bytes,
        PipelineStage::Overlay,
    )?
    .maximum_working_bytes();
    let progress_bytes = usize_to_u64(overlay.byte_size());
    let inputs = [StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(high_water_bytes, progress_bytes, item_deadline),
        ),
        ScipOverlayWork { facts, overlay },
    )];
    let maximum_generation_bytes = config.limits.retained.max_generation_bytes;
    let source_limits = config.limits.source_limits;
    let evidence_policy = config.evidence_policy();
    let source_root = stages.source_root.clone();
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Overlay, StageCapacity::new(1, 0), deadline),
        StageWorkload::new(inputs, move |item: StageWorkItem<u8, ScipOverlayWork>| {
            let source_root = source_root.clone();
            async move {
                let cancellation = item.cancellation();
                let (_, _, work) = item.into_parts();
                block_in_place(move || {
                    apply_scip_overlay_work(
                        ScipOverlayExecution {
                            work,
                            source_root,
                            source_limits,
                            maximum_generation_bytes,
                            high_water_bytes,
                            evidence_policy,
                        },
                        &cancellation,
                    )
                })
            }
        }),
        StageFold::new(
            None,
            |result: &mut Option<ScipOverlayStageOutput>,
             output: StageOutput<u8, ScipOverlayStageOutput>| {
                let (_, output) = output.into_parts();
                *result = Some(output);
                Ok(())
            },
        ),
    );
    stages
        .runner
        .execute(execution)
        .await?
        .ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Overlay,
        })
}

struct ScipOverlayExecution {
    work: ScipOverlayWork,
    source_root: SourceRoot,
    source_limits: SourceLimits,
    maximum_generation_bytes: u64,
    high_water_bytes: u64,
    evidence_policy: NativeEvidencePolicy,
}

fn apply_scip_overlay_work(
    execution: ScipOverlayExecution,
    cancellation: &crate::StageCancellation,
) -> Result<ScipOverlayStageOutput, StageItemFailure> {
    let ScipOverlayExecution {
        work,
        source_root,
        source_limits,
        maximum_generation_bytes,
        high_water_bytes,
        evidence_policy,
    } = execution;
    let ScipOverlayWork { mut facts, overlay } = work;
    let source_cancellation = cancellation.clone();
    let overlay_cancellation = cancellation.clone();
    let report = apply_scip_overlay_with_cancellation(
        ScipOverlayRequest::new(&mut facts, &overlay.bytes, overlay.maximum_rows),
        |raw_path| {
            let path = NormalizedPath::parse(raw_path).ok()?;
            let snapshot = source_root
                .read_with_cancellation(
                    &path,
                    SourceReadOptions::new(source_limits, || source_cancellation.is_cancelled()),
                )
                .ok()?;
            Some(String::from(snapshot.into_source()).into_bytes())
        },
        || overlay_cancellation.is_cancelled(),
    )
    .map_err(|_| StageItemFailure)?;
    if evidence_policy.centrality.page_rank {
        apply_page_rank(&mut facts, || cancellation.is_cancelled())
            .map_err(|_| StageItemFailure)?;
    }
    if evidence_policy.centrality.betweenness {
        apply_sampled_betweenness(&mut facts, || cancellation.is_cancelled())
            .map_err(|_| StageItemFailure)?;
    }
    if !evidence_policy.retention.call_sites {
        facts.references.clear();
    }
    let measurement = facts
        .measure_retained_bytes(maximum_generation_bytes, || cancellation.is_cancelled())
        .map_err(|_| StageItemFailure)?;
    Ok(ScipOverlayStageOutput {
        facts,
        report,
        retained_bytes: measurement.retained_bytes(),
        high_water_bytes,
    })
}

async fn run_reduce_stage(
    stages: &NativeStageContext<'_>,
    facts: GenerationFacts,
    progress_bytes: u64,
) -> Result<(CanonicalGenerationFacts, GenerationValidationReport), NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let item_deadline = planned_item_deadline(config.deadlines.item_timeout, deadline.deadline());
    let validation_limits = generation_validation_limits(
        config.limits.retained.max_generation_bytes,
        PipelineStage::Reduce,
    )?;
    let inputs = [StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(
                validation_limits.maximum_working_bytes(),
                progress_bytes,
                item_deadline,
            ),
        ),
        facts,
    )];
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Reduce, StageCapacity::new(1, 0), deadline),
        StageWorkload::new(
            inputs,
            move |item: StageWorkItem<u8, GenerationFacts>| async move {
                let cancellation = item.cancellation();
                let (_, _, facts) = item.into_parts();
                block_in_place(move || {
                    Ok::<_, StageItemFailure>(validate_generation_facts(
                        facts,
                        validation_limits,
                        || cancellation.is_cancelled(),
                    ))
                })
            },
        ),
        StageFold::new(
            None,
            |reduced: &mut Option<
                Result<
                    (CanonicalGenerationFacts, GenerationValidationReport),
                    GenerationValidationError,
                >,
            >,
             output: StageOutput<
                u8,
                Result<
                    (CanonicalGenerationFacts, GenerationValidationReport),
                    GenerationValidationError,
                >,
            >| {
                let (_, output) = output.into_parts();
                *reduced = Some(output);
                Ok(())
            },
        ),
    );
    let result =
        stages
            .runner
            .execute(execution)
            .await?
            .ok_or(NativePipelineError::Incomplete {
                stage: PipelineStage::Reduce,
            })?;
    result.map_err(|error| NativePipelineError::Validation {
        reason: generation_validation_failure_reason(&error),
    })
}

async fn run_spilled_reduce_stage(
    stages: &NativeStageContext<'_>,
    spill: NativeGenerationSpill,
) -> Result<NativeGenerationSpillDigestReport, NativePipelineError> {
    let config = stages.config;
    let deadline = config.stage_deadlines();
    let item_deadline = deadline.deadline();
    let inputs = [StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            0_u8,
            StageItemBudget::new(SPILLED_REDUCTION_RESERVATION_BYTES, 0, item_deadline),
        ),
        spill,
    )];
    let progress = stages.runner.clone();
    let failure = SpilledReduceFailure::default();
    let worker_failure = failure.clone();
    let execution = StageExecution::new(
        StageRunConfig::new(PipelineStage::Reduce, StageCapacity::new(1, 0), deadline),
        StageWorkload::new(
            inputs,
            move |item: StageWorkItem<u8, NativeGenerationSpill>| {
                let progress = progress.clone();
                let failure = worker_failure.clone();
                async move {
                    let cancellation = item.cancellation();
                    let (_, _, spill) = item.into_parts();
                    loop {
                        if cancellation.is_cancelled() {
                            return Err(StageItemFailure);
                        }
                        let partition = spill
                            .canonicalize_next()
                            .await
                            .map_err(|error| failure.record(error))?;
                        progress
                            .advance_progress(1, 0)
                            .await
                            .map_err(|_| StageItemFailure)?;
                        if partition.complete {
                            break;
                        }
                    }
                    let digest_progress = progress.clone();
                    spill
                        .compute_digest(|| cancellation.is_cancelled(), move |rows| {
                            let digest_progress = digest_progress.clone();
                            async move {
                                digest_progress.advance_progress(rows, 0).await.is_ok()
                            }
                        })
                        .await
                        .map_err(|error| failure.record(error))
                }
            },
        ),
        StageFold::new(
            None,
            |digest: &mut Option<NativeGenerationSpillDigestReport>,
             output: StageOutput<u8, NativeGenerationSpillDigestReport>| {
                let (_, output) = output.into_parts();
                *digest = Some(output);
                Ok(())
            },
        ),
    );
    stages
        .runner
        .execute(execution)
        .await
        .map_err(|error| failure.classify(error))?
        .ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Reduce,
        })
}

/// The storage failure that ended the spilled reduce item.
///
/// The stage runner only learns that the item failed; retaining the storage
/// error lets the stage report a statement timeout or capacity rejection as
/// its actionable reason instead of an opaque item failure.
#[derive(Clone, Default)]
struct SpilledReduceFailure(Arc<Mutex<Option<StorageError>>>);

impl SpilledReduceFailure {
    fn record(&self, error: StorageError) -> StageItemFailure {
        if let Ok(mut retained) = self.0.lock() {
            retained.get_or_insert(error);
        }
        StageItemFailure
    }

    fn classify(&self, error: StageRunError) -> NativePipelineError {
        let worker_failed = matches!(
            error,
            StageRunError::Item {
                stage: PipelineStage::Reduce,
                kind: StageFailureKind::Worker,
                ..
            }
        );
        let retained = worker_failed
            .then(|| self.0.lock().ok().and_then(|mut retained| retained.take()))
            .flatten();
        retained.map_or(NativePipelineError::Stage(error), |storage| {
            spill_pipeline_error(PipelineStage::Reduce, &storage)
        })
    }
}

fn generation_validation_failure_reason(
    error: &GenerationValidationError,
) -> Option<PipelineFailureReason> {
    match error {
        GenerationValidationError::ReferenceNameTooLong => {
            Some(PipelineFailureReason::ReferenceNameTooLong)
        }
        GenerationValidationError::RetainedLimit => {
            Some(PipelineFailureReason::GenerationCapacityExceeded)
        }
        GenerationValidationError::Storage(StorageError::InvalidInput { field }) => {
            Some(PipelineFailureReason::CanonicalFieldRejected(field))
        }
        GenerationValidationError::Storage(_) | GenerationValidationError::Cancelled => None,
    }
}

struct ReadEnvelopeIterator<Inputs> {
    sources: Inputs,
    sequence: u64,
    item_timeout: Duration,
    stage_deadline: Instant,
    maximum_source_bytes: u64,
}

impl<Inputs> Iterator for ReadEnvelopeIterator<Inputs>
where
    Inputs: Iterator<Item = DiscoveredSource>,
{
    type Item = StageEnvelope<NormalizedPath, DiscoveredSource>;

    fn next(&mut self) -> Option<Self::Item> {
        let source = self.sources.next()?;
        let admitted_bytes = source.byte_size().min(self.maximum_source_bytes);
        let reserved_bytes = native_read_reservation(admitted_bytes).unwrap_or(u64::MAX);
        let sequence = StageSequence::new(self.sequence);
        self.sequence = self.sequence.saturating_add(1);
        let key = source.path().clone();
        let item_deadline = planned_item_deadline(self.item_timeout, self.stage_deadline);
        Some(StageEnvelope::new(
            StageItemMeta::new(
                sequence,
                key,
                StageItemBudget::new(reserved_bytes, source.byte_size(), item_deadline),
            ),
            source,
        ))
    }
}

struct ParseEnvelopeIterator<Inputs> {
    entries: Inputs,
    sequence: u64,
    item_timeout: Duration,
    stage_deadline: Instant,
}

impl<Inputs> Iterator for ParseEnvelopeIterator<Inputs>
where
    Inputs: Iterator<Item = SourceManifestEntry>,
{
    type Item = StageEnvelope<FileId, SourceManifestEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.entries.next()?;
        let reserved_bytes = native_extraction_reservation(entry.byte_size).unwrap_or(u64::MAX);
        let sequence = StageSequence::new(self.sequence);
        self.sequence = self.sequence.saturating_add(1);
        let key = entry.file_id.clone();
        let item_deadline = planned_item_deadline(self.item_timeout, self.stage_deadline);
        Some(StageEnvelope::new(
            StageItemMeta::new(
                sequence,
                key,
                StageItemBudget::new(reserved_bytes, entry.byte_size, item_deadline),
            ),
            entry,
        ))
    }
}

#[derive(Debug)]
struct SourceManifestEntry {
    path: NormalizedPath,
    language: SourceLanguage,
    file_id: FileId,
    content_hash: ContentDigest,
    byte_size: u64,
}

impl SourceManifestEntry {
    fn modeled_retained_bytes(&self) -> u64 {
        usize_to_u64(size_of::<Self>())
            .saturating_mul(2)
            .saturating_add(usize_to_u64(self.path.as_str().len()))
            .saturating_add(usize_to_u64(self.file_id.as_str().len()))
            .saturating_add(usize_to_u64(self.content_hash.as_str().len()))
    }
}

struct SourceManifest {
    entries: Vec<SourceManifestEntry>,
    retained_bytes: u64,
    source_bytes: u64,
    maximum_bytes: u64,
}

impl SourceManifest {
    const fn new(maximum_bytes: u64) -> Self {
        Self {
            entries: Vec::new(),
            retained_bytes: 0,
            source_bytes: 0,
            maximum_bytes,
        }
    }

    fn push(&mut self, entry: SourceManifestEntry) -> Result<(), StageItemFailure> {
        let retained_bytes = self
            .retained_bytes
            .checked_add(entry.modeled_retained_bytes())
            .ok_or(StageItemFailure)?;
        let source_bytes = self
            .source_bytes
            .checked_add(entry.byte_size)
            .ok_or(StageItemFailure)?;
        if retained_bytes > self.maximum_bytes {
            return Err(StageItemFailure);
        }
        self.entries
            .try_reserve_exact(1)
            .map_err(|_| StageItemFailure)?;
        self.entries.push(entry);
        self.retained_bytes = retained_bytes;
        self.source_bytes = source_bytes;
        Ok(())
    }
}

struct ReadManifestInput {
    source_root: SourceRoot,
    discovered: DiscoveredSource,
    global_limits: SourceLimits,
}

fn read_manifest_entry<Cancel>(
    input: ReadManifestInput,
    cancelled: Cancel,
) -> Result<Option<SourceManifestEntry>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ReadManifestInput {
        source_root,
        discovered,
        global_limits,
    } = input;
    let exact_limits = exact_source_limits(discovered.byte_size(), global_limits)?;
    let snapshot = match source_root.read_with_cancellation(
        discovered.path(),
        SourceReadOptions::new(exact_limits, cancelled),
    ) {
        Ok(snapshot) => snapshot,
        Err(SourceReadError::UnsupportedLanguage) => return Ok(None),
        Err(_) => return Err(StageItemFailure),
    };
    if snapshot.byte_size() != discovered.byte_size() {
        return Err(StageItemFailure);
    }
    Ok(Some(SourceManifestEntry {
        path: snapshot.path().clone(),
        language: snapshot.language(),
        file_id: snapshot.file_id().clone(),
        content_hash: snapshot.content_hash().clone(),
        byte_size: snapshot.byte_size(),
    }))
}

struct ParseSourceRequest<'input> {
    source_root: &'input SourceRoot,
    manifest: SourceManifestEntry,
    maximum_ast_depth: usize,
}

fn parse_manifest_entry<Cancel>(
    request: &ParseSourceRequest<'_>,
    mut cancelled: Cancel,
) -> Result<ExtractedFile, ParseManifestFailure>
where
    Cancel: FnMut() -> bool,
{
    let snapshot = read_manifest_snapshot(request.source_root, &request.manifest, &mut cancelled)?;
    let path = snapshot.path().clone();
    let mut extractor = NativeExtractor::new(snapshot.language())
        .and_then(|extractor| extractor.with_maximum_ast_depth(request.maximum_ast_depth))
        .map_err(|error| ParseManifestFailure::from_extract(error).with_path(path.clone()))?;
    extractor
        .extract_with_cancellation(&snapshot, cancelled)
        .map_err(|error| ParseManifestFailure::from_extract(error).with_path(path))
}

fn read_manifest_snapshot<Cancel>(
    source_root: &SourceRoot,
    manifest: &SourceManifestEntry,
    cancelled: &mut Cancel,
) -> Result<SourceSnapshot, ParseManifestFailure>
where
    Cancel: FnMut() -> bool,
{
    let path = manifest.path.clone();
    let ceiling = exact_limit_ceiling(manifest.byte_size)
        .map_err(|_| ParseManifestFailure::unclassified().with_path(path.clone()))?;
    let exact_limits = exact_source_limits(manifest.byte_size, ceiling)
        .map_err(|_| ParseManifestFailure::unclassified().with_path(path.clone()))?;
    let snapshot = source_root
        .read_with_cancellation(
            &manifest.path,
            SourceReadOptions::new(exact_limits, cancelled),
        )
        .map_err(|error| ParseManifestFailure::from_source_read(error).with_path(path.clone()))?;
    if snapshot.byte_size() != manifest.byte_size
        || snapshot.content_hash() != &manifest.content_hash
        || snapshot.file_id() != &manifest.file_id
        || snapshot.language() != manifest.language
    {
        return Err(ParseManifestFailure::source_changed().with_path(path));
    }
    Ok(snapshot)
}

struct SpilledExtractorPool {
    extractors: HashMap<SourceLanguage, NativeExtractor>,
    maximum_ast_depth: usize,
    #[cfg(test)]
    initializations: usize,
}

impl Default for SpilledExtractorPool {
    fn default() -> Self {
        Self::new(DEFAULT_MAXIMUM_AST_DEPTH)
    }
}

impl SpilledExtractorPool {
    fn new(maximum_ast_depth: usize) -> Self {
        Self {
            extractors: HashMap::new(),
            maximum_ast_depth,
            #[cfg(test)]
            initializations: 0,
        }
    }

    fn extract<Cancel>(
        &mut self,
        source_root: &SourceRoot,
        manifest: &SourceManifestEntry,
        mut cancelled: Cancel,
    ) -> Result<ExtractedFile, ParseManifestFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let snapshot = read_manifest_snapshot(source_root, manifest, &mut cancelled)?;
        let path = snapshot.path().clone();
        let language = snapshot.language();
        if !self.extractors.contains_key(&language) {
            self.extractors
                .try_reserve(1)
                .map_err(|_| ParseManifestFailure::unclassified().with_path(path.clone()))?;
            let extractor = NativeExtractor::new(language)
                .and_then(|extractor| extractor.with_maximum_ast_depth(self.maximum_ast_depth))
                .map_err(|error| {
                    ParseManifestFailure::from_extract(error).with_path(path.clone())
                })?;
            if self.extractors.insert(language, extractor).is_some() {
                return Err(ParseManifestFailure::unclassified().with_path(path));
            }
            #[cfg(test)]
            {
                self.initializations = self.initializations.saturating_add(1);
            }
        }
        self.extractors
            .get_mut(&language)
            .ok_or_else(|| ParseManifestFailure::unclassified().with_path(path.clone()))?
            .extract_with_cancellation(&snapshot, cancelled)
            .map_err(|error| ParseManifestFailure::from_extract(error).with_path(path))
    }
}

fn exact_source_limits(
    observed_bytes: u64,
    global_limits: SourceLimits,
) -> Result<SourceLimits, StageItemFailure> {
    if observed_bytes > usize_to_u64(global_limits.max_source_bytes()) {
        return Err(StageItemFailure);
    }
    let exact = usize::try_from(observed_bytes.max(1)).map_err(|_| StageItemFailure)?;
    SourceLimits::new(exact).map_err(|_| StageItemFailure)
}

fn exact_limit_ceiling(observed_bytes: u64) -> Result<SourceLimits, StageItemFailure> {
    let exact = usize::try_from(observed_bytes.max(1)).map_err(|_| StageItemFailure)?;
    SourceLimits::new(exact).map_err(|_| StageItemFailure)
}

struct NativeFactAccumulator {
    files: Vec<NativeFileFacts>,
    retained_bytes: u64,
    maximum_bytes: u64,
    diagnostics: u64,
    degraded_files: Vec<NativeDegradedFile>,
    degraded_files_truncated: u64,
}

impl NativeFactAccumulator {
    const fn new(maximum_bytes: u64) -> Self {
        Self {
            files: Vec::new(),
            retained_bytes: 0,
            maximum_bytes,
            diagnostics: 0,
            degraded_files: Vec::new(),
            degraded_files_truncated: 0,
        }
    }

    fn push(&mut self, extracted: ExtractedFile) -> Result<(), StageItemFailure> {
        record_degraded_file(
            &mut self.degraded_files,
            &mut self.degraded_files_truncated,
            &extracted,
        )?;
        let diagnostics = self
            .diagnostics
            .checked_add(usize_to_u64(extracted.diagnostics.len()))
            .ok_or(StageItemFailure)?;
        let file = NativeFileFacts::from_extracted(extracted)?;
        self.push_native(file, diagnostics)
    }

    fn push_native(
        &mut self,
        file: NativeFileFacts,
        diagnostics: u64,
    ) -> Result<(), StageItemFailure> {
        let file_bytes = file.modeled_retained_bytes();
        self.push_native_measured(file, diagnostics, file_bytes)
    }

    fn push_compact_clone(
        &mut self,
        file: NativeFileFacts,
        diagnostics: u64,
    ) -> Result<(), StageItemFailure> {
        let file_bytes = file.modeled_owned_bytes();
        self.push_native_measured(file, diagnostics, file_bytes)
    }

    fn push_native_measured(
        &mut self,
        file: NativeFileFacts,
        diagnostics: u64,
        file_bytes: u64,
    ) -> Result<(), StageItemFailure> {
        let minimum_retained = self
            .retained_bytes
            .checked_add(file_bytes)
            .and_then(|bytes| bytes.checked_add(usize_to_u64(size_of::<NativeFileFacts>())))
            .ok_or(StageItemFailure)?;
        if minimum_retained > self.maximum_bytes {
            return Err(StageItemFailure);
        }
        let old_capacity = self.files.capacity();
        self.files
            .try_reserve_exact(1)
            .map_err(|_| StageItemFailure)?;
        let added_capacity = self.files.capacity().saturating_sub(old_capacity);
        let retained_bytes = self
            .retained_bytes
            .checked_add(file_bytes)
            .and_then(|bytes| {
                bytes.checked_add(
                    usize_to_u64(added_capacity)
                        .saturating_mul(usize_to_u64(size_of::<NativeFileFacts>())),
                )
            })
            .ok_or(StageItemFailure)?;
        if retained_bytes > self.maximum_bytes {
            return Err(StageItemFailure);
        }
        self.files.push(file);
        self.retained_bytes = retained_bytes;
        self.diagnostics = diagnostics;
        Ok(())
    }
}

fn record_degraded_file(
    reported: &mut Vec<NativeDegradedFile>,
    truncated: &mut u64,
    file: &ExtractedFile,
) -> Result<(), StageItemFailure> {
    let reason = file
        .diagnostics
        .iter()
        .find_map(|diagnostic| degraded_file_reason(diagnostic.code));
    let Some(reason) = reason else {
        return Ok(());
    };
    if reported.len() >= MAX_REPORTED_DEGRADED_FILES {
        *truncated = truncated.checked_add(1).ok_or(StageItemFailure)?;
        return Ok(());
    }
    reported
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    reported.push(NativeDegradedFile {
        path: try_clone_text(file.path.as_str())?,
        reason,
    });
    Ok(())
}

const fn degraded_file_reason(diagnostic: DiagnosticCode) -> Option<&'static str> {
    match diagnostic {
        DiagnosticCode::InvalidSpan => Some("extraction_invalid_span"),
        DiagnosticCode::ParserStopped => Some("extraction_parser_stopped"),
        DiagnosticCode::NestingLimitExceeded => Some("nesting_limit_exceeded"),
        DiagnosticCode::SyntaxError
        | DiagnosticCode::CanonicalNameTruncated
        | DiagnosticCode::OptionalFactsOmitted => None,
    }
}

fn append_degraded_files(
    reported: &mut Vec<NativeDegradedFile>,
    truncated: &mut u64,
    incoming: &mut Vec<NativeDegradedFile>,
) -> Result<(), StageItemFailure> {
    let available = MAX_REPORTED_DEGRADED_FILES.saturating_sub(reported.len());
    let admitted = available.min(incoming.len());
    reported
        .try_reserve_exact(admitted)
        .map_err(|_| StageItemFailure)?;
    reported.extend(incoming.drain(..admitted));
    *truncated = truncated
        .checked_add(usize_to_u64(incoming.len()))
        .ok_or(StageItemFailure)?;
    incoming.clear();
    Ok(())
}

fn compact_clone_file(mut file: NativeFileFacts) -> NativeFileFacts {
    file.containments = Vec::new();
    file.references = Vec::new();
    file.call_scope_sites = Vec::new();
    file.receiver_evidence = None;
    file.numerical_sites = Vec::new();
    file.import_bindings = Vec::new();
    file.test_search_text = String::new();
    for symbol in &mut file.symbols {
        symbol.name = String::new();
        symbol.input.symbol_kind = String::new();
        symbol.input.qualified_name = String::new();
        symbol.input.signature = String::new();
        symbol.docstring = None;
        symbol.body_search_text = String::new();
    }
    file
}

struct NativeFileFacts {
    file: FileInput,
    line_count: u32,
    symbols: Vec<NativeSymbolFacts>,
    containments: Vec<Containment>,
    references: Vec<ExtractedReference>,
    call_scope_sites: Vec<ExtractedCallScopeSite>,
    javascript_member_calls: Vec<JavascriptMemberCallContext>,
    resolution_abstentions: Vec<SourceSpan>,
    local_type_scopes: Vec<(SourceSpan, SourceSpan)>,
    receiver_evidence: Option<Box<ExtractedReceiverEvidence>>,
    numerical_sites: Vec<ExtractedNumericalSite>,
    import_bindings: Vec<ExtractedImportBinding>,
    has_inline_tests: bool,
    test_search_text: String,
    test_search_truncated: bool,
}

impl NativeFileFacts {
    fn from_extracted(extracted: ExtractedFile) -> Result<Self, StageItemFailure> {
        let ExtractedFile {
            file_id,
            path,
            language,
            content_hash,
            byte_size,
            line_count,
            parse_status,
            symbols,
            containments,
            references,
            call_scope_sites,
            javascript_member_calls,
            resolution_abstentions,
            local_type_scopes,
            receiver_evidence,
            numerical_sites,
            import_bindings,
            has_inline_tests,
            test_search_text,
            test_search_truncated,
            diagnostics: _,
        } = extracted;
        let mut normalized_symbols = Vec::new();
        normalized_symbols
            .try_reserve(symbols.len())
            .map_err(|_| StageItemFailure)?;
        for symbol in symbols {
            normalized_symbols.push(normalize_native_symbol(&file_id, language, symbol));
        }
        Ok(Self {
            file: FileInput {
                file_id,
                normalized_path: path.as_str().to_owned(),
                language: language.as_str().to_owned(),
                content_hash,
                byte_size,
                parse_status,
            },
            line_count,
            symbols: normalized_symbols,
            containments,
            references,
            call_scope_sites,
            javascript_member_calls,
            resolution_abstentions,
            local_type_scopes,
            receiver_evidence,
            numerical_sites,
            import_bindings,
            has_inline_tests,
            test_search_text,
            test_search_truncated,
        })
    }

    fn modeled_owned_bytes(&self) -> u64 {
        let mut bytes = usize_to_u64(size_of::<Self>())
            .saturating_add(modeled_file_input_bytes(&self.file))
            .saturating_add(vector_capacity_bytes(&self.symbols))
            .saturating_add(vector_capacity_bytes(&self.containments))
            .saturating_add(vector_capacity_bytes(&self.references))
            .saturating_add(generic_resolution::call_scope_owned_bytes(
                &self.call_scope_sites,
            ))
            .saturating_add(vector_capacity_bytes(&self.javascript_member_calls))
            .saturating_add(vector_capacity_bytes(&self.resolution_abstentions))
            .saturating_add(vector_capacity_bytes(&self.local_type_scopes))
            .saturating_add(receiver_resolution::evidence_bytes(
                self.receiver_evidence.as_deref(),
            ))
            .saturating_add(vector_capacity_bytes(&self.numerical_sites))
            .saturating_add(vector_capacity_bytes(&self.import_bindings))
            .saturating_add(usize_to_u64(self.test_search_text.capacity()));
        for symbol in &self.symbols {
            bytes = bytes.saturating_add(symbol.modeled_retained_bytes());
        }
        for containment in &self.containments {
            bytes = bytes
                .saturating_add(usize_to_u64(containment.parent.as_str().len()))
                .saturating_add(usize_to_u64(containment.child.as_str().len()));
        }
        for reference in &self.references {
            bytes = bytes
                .saturating_add(
                    reference
                        .owner
                        .as_ref()
                        .map_or(0, |owner| usize_to_u64(owner.as_str().len())),
                )
                .saturating_add(usize_to_u64(reference.name.capacity()))
                .saturating_add(usize_to_u64(
                    reference
                        .resolution_name
                        .as_ref()
                        .map_or(0, String::capacity),
                ));
        }
        for call in &self.javascript_member_calls {
            if let JavascriptMemberReceiver::Constructor(name) = &call.receiver {
                bytes = bytes.saturating_add(usize_to_u64(name.capacity()));
            }
        }
        for site in &self.numerical_sites {
            bytes = bytes
                .saturating_add(usize_to_u64(site.id.as_str().len()))
                .saturating_add(
                    site.owner
                        .as_ref()
                        .map_or(0, |owner| usize_to_u64(owner.as_str().len())),
                )
                .saturating_add(usize_to_u64(site.operation.capacity()))
                .saturating_add(usize_to_u64(site.hazard.capacity()))
                .saturating_add(usize_to_u64(site.precision.capacity()))
                .saturating_add(usize_to_u64(site.expression_digest.as_str().len()))
                .saturating_add(usize_to_u64(site.provenance.capacity()))
                .saturating_add(usize_to_u64(site.unknowns.capacity()));
        }
        for binding in &self.import_bindings {
            bytes = bytes
                .saturating_add(usize_to_u64(binding.module_specifier.capacity()))
                .saturating_add(usize_to_u64(binding.imported_name.capacity()))
                .saturating_add(usize_to_u64(binding.local_name.capacity()));
        }
        bytes
    }

    fn modeled_retained_bytes(&self) -> u64 {
        self.modeled_owned_bytes()
            .saturating_add(self.anticipated_output_bytes())
    }

    fn anticipated_output_bytes(&self) -> u64 {
        let path_bytes = usize_to_u64(self.file.normalized_path.len());
        let language_bytes = usize_to_u64(self.file.language.len());
        let mut bytes = UUID_TEXT_BYTES
            .saturating_add(path_bytes)
            .saturating_add(language_bytes)
            .saturating_add(anticipated_file_document_bytes(&self.file))
            .saturating_add(usize_to_u64(self.test_search_text.len()))
            .saturating_add(anticipated_file_symbol_bytes(&self.file));
        for containment in &self.containments {
            bytes = bytes
                .saturating_add(usize_to_u64(containment.parent.as_str().len()))
                .saturating_add(usize_to_u64(containment.child.as_str().len()))
                .saturating_add(usize_to_u64(CONTAINMENT_PROVENANCE.len()));
        }
        for reference in &self.references {
            bytes = bytes
                .saturating_add(reference.owner.as_ref().map_or(UUID_TEXT_BYTES, |owner| {
                    usize_to_u64(owner.as_str().len()).saturating_mul(2)
                }))
                .saturating_add(UUID_TEXT_BYTES.saturating_mul(3))
                .saturating_add(usize_to_u64(reference.name.len()))
                .saturating_add(usize_to_u64(reference.kind.as_str().len()))
                .saturating_add(MAX_RESOLUTION_PROVENANCE_BYTES.saturating_mul(2));
        }
        for site in &self.numerical_sites {
            bytes = bytes
                .saturating_add(usize_to_u64(size_of::<NumericalSiteInput>()))
                .saturating_add(usize_to_u64(site.id.as_str().len()))
                .saturating_add(UUID_TEXT_BYTES.saturating_mul(2))
                .saturating_add(usize_to_u64(site.operation.len()))
                .saturating_add(usize_to_u64(site.hazard.len()))
                .saturating_add(usize_to_u64(site.precision.len()))
                .saturating_add(usize_to_u64(site.expression_digest.as_str().len()))
                .saturating_add(usize_to_u64(site.provenance.len()))
                .saturating_add(usize_to_u64("heuristic".len()))
                .saturating_add(usize_to_u64(site.unknowns.len()));
        }
        for symbol in &self.symbols {
            bytes = bytes.saturating_add(anticipated_document_bytes(
                symbol,
                path_bytes,
                language_bytes,
            ));
            bytes = bytes
                .saturating_add(usize_to_u64(size_of::<EdgeInput>()))
                .saturating_add(UUID_TEXT_BYTES.saturating_mul(2))
                .saturating_add(usize_to_u64(FILE_CONTAINMENT_PROVENANCE.len()));
        }
        bytes
    }
}

fn normalize_native_symbol(
    file_id: &FileId,
    language: SourceLanguage,
    symbol: cartograph_extract::ExtractedSymbol,
) -> NativeSymbolFacts {
    let cartograph_extract::ExtractedSymbol {
        id,
        kind,
        name,
        qualified_name,
        span,
        signature,
        docstring,
        body_search_text,
        body_search_truncated,
        health,
        implementation,
        export,
        execution,
        declaration_syntax,
        visibility,
        structural_digest,
        clone_shape_digest,
        clone_token_profile,
    } = symbol;
    let augmentation = language == SourceLanguage::GraphQl
        && signature
            .as_deref()
            .is_some_and(|value| value.starts_with("extend "));
    NativeSymbolFacts {
        input: SymbolInput {
            symbol_id: id,
            file_id: file_id.clone(),
            symbol_kind: kind.as_str().to_owned(),
            qualified_name,
            signature: persisted_signature(kind, signature),
            start_byte: span.start_byte(),
            end_byte: span.end_byte(),
            start_line: span.start_line(),
            end_line: span.end_line(),
            structural_digest,
            visibility,
            export,
            execution,
            declaration_only: implementation.declaration_only,
            betweenness_ppb: None,
            pagerank_ppb: None,
        },
        kind,
        name,
        docstring,
        body_search_text,
        body_search_truncated,
        health,
        implementation,
        export,
        execution,
        declaration_syntax,
        visibility,
        clone_shape_digest,
        clone_token_profile,
        duplicate_detection_enabled: true,
        near_clone_compatibility: NearCloneCompatibility::Unclaimed,
        partial_clone: None,
        augmentation,
    }
}

fn persisted_signature(kind: SymbolKind, signature: Option<String>) -> String {
    signature
        .filter(|value| symbol_signature_is_search_safe(kind, value))
        .unwrap_or_default()
}

struct NativeSymbolFacts {
    input: SymbolInput,
    kind: SymbolKind,
    name: String,
    docstring: Option<String>,
    body_search_text: String,
    body_search_truncated: bool,
    health: cartograph_extract::SymbolHealthMetrics,
    implementation: SymbolImplementationFlags,
    export: SymbolExportFlags,
    execution: SymbolExecutionFlags,
    declaration_syntax: DeclarationSyntax,
    visibility: Option<Visibility>,
    clone_shape_digest: ContentDigest,
    clone_token_profile: Option<CloneTokenProfile>,
    duplicate_detection_enabled: bool,
    near_clone_compatibility: NearCloneCompatibility,
    partial_clone: Option<PartialCloneEvidence>,
    augmentation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NearCloneCompatibility {
    Unclaimed,
    Compatible,
}

impl NearCloneCompatibility {
    const fn is_compatible(self) -> bool {
        matches!(self, Self::Compatible)
    }
}

struct PartialCloneEvidence {
    peer_count: u32,
    component_size: u32,
    maximum_overlap_ppm: u32,
    minimum_overlap_ppm: u32,
    representative: SymbolId,
    listed_peers: Vec<SymbolId>,
}

impl NativeSymbolFacts {
    fn modeled_retained_bytes(&self) -> u64 {
        usize_to_u64(size_of::<Self>())
            .saturating_add(modeled_symbol_input_bytes(&self.input))
            .saturating_add(usize_to_u64(self.name.capacity()))
            .saturating_add(usize_to_u64(self.body_search_text.capacity()))
            .saturating_add(usize_to_u64(self.clone_shape_digest.as_str().len()))
            .saturating_add(
                self.clone_token_profile
                    .as_ref()
                    .map_or(0, |profile| usize_to_u64(profile.retained_bytes())),
            )
            .saturating_add(self.partial_clone.as_ref().map_or(0, |evidence| {
                vector_capacity_bytes(&evidence.listed_peers)
                    .saturating_add(usize_to_u64(evidence.representative.as_str().len()))
                    .saturating_add(
                        evidence
                            .listed_peers
                            .iter()
                            .map(|peer| usize_to_u64(peer.as_str().len()))
                            .sum(),
                    )
            }))
            .saturating_add(
                self.docstring
                    .as_ref()
                    .map_or(0, |docstring| usize_to_u64(docstring.capacity())),
            )
    }
}

#[derive(Clone)]
struct ResolutionCandidate {
    file_id: FileId,
    symbol_id: SymbolId,
    parent_symbol_id: Option<SymbolId>,
    qualified_name: String,
    signature: String,
    kind: SymbolKind,
    visibility: Option<Visibility>,
    implementation: SymbolImplementationFlags,
    export: SymbolExportFlags,
    top_level: bool,
    augmentation: bool,
    static_member: bool,
    declaration_syntax: DeclarationSyntax,
    declaration_span: (u64, u64),
}

#[derive(Clone)]
struct ProjectExport {
    symbol_id: SymbolId,
    default_export: bool,
}

struct ProjectReExport {
    source_file_id: FileId,
    source_symbol_id: SymbolId,
    module_specifier: String,
    namespace: bool,
}

#[derive(Clone)]
struct RustPackageRoot {
    directory: String,
    entry_file_id: FileId,
}

struct RustNamedReExport {
    source_file_id: FileId,
    public_name: String,
    module_specifier: String,
}

struct TestFileEvidence {
    file_id: FileId,
    import_specifiers: Vec<String>,
    has_inline_tests: bool,
}

type CandidateMap = HashMap<String, ResolutionCandidateBucket>;
type DefaultExportMap = HashMap<FileId, Vec<ResolutionCandidate>>;
type ParentMap = HashMap<SymbolId, SymbolId>;
type ModulePathMap = HashMap<String, Vec<FileId>>;
type FileResolutionContextMap = BTreeMap<FileId, ResolutionFileContext>;
type FileSymbolMap = HashMap<FileId, SymbolId>;
type FileOrdinalMap = HashMap<FileId, u64>;
type FileExportMap = HashMap<FileId, BTreeMap<String, Option<ProjectExport>>>;
type RustPackageMap = HashMap<String, Option<RustPackageRoot>>;

#[derive(Clone, Copy)]
struct ResolutionCandidateRange {
    file_ordinal: u64,
    start: usize,
    end: usize,
}

#[derive(Default)]
struct ResolutionCandidateBucket {
    native_bridge_member: bool,
    candidates: Vec<ResolutionCandidate>,
    by_file: Vec<ResolutionCandidateRange>,
    globally_visible: Vec<usize>,
    non_visible_by_language: HashMap<String, Vec<usize>>,
}

impl ResolutionCandidateBucket {
    fn as_slice(&self) -> &[ResolutionCandidate] {
        &self.candidates
    }

    fn iter(&self) -> impl Iterator<Item = &ResolutionCandidate> {
        self.candidates.iter()
    }

    fn for_file(&self, file_ordinal: u64) -> &[ResolutionCandidate] {
        let Ok(range_index) = self
            .by_file
            .binary_search_by_key(&file_ordinal, |range| range.file_ordinal)
        else {
            return &[];
        };
        let range = &self.by_file[range_index];
        self.candidates
            .get(range.start..range.end)
            .unwrap_or_default()
    }
}

struct ResolutionFileContext {
    path: String,
    directory: String,
    language: String,
    package: Option<String>,
}

#[derive(Default)]
struct ModulePathIndex {
    exact: ModulePathMap,
    stem: ModulePathMap,
    directory_index: ModulePathMap,
    declared_units: ModulePathMap,
    implemented_units: ModulePathMap,
    files: FileResolutionContextMap,
    rust_packages: RustPackageMap,
    typescript_aliases: TypeScriptAliasIndex,
    python_source_roots: python_resolution::SourceRootIndex,
    javascript_configs: javascript_config::ConfigIndex,
    javascript_packages: javascript_packages::PackageIndex,
    file_paths: file_path_resolution::SuffixIndex,
}

#[derive(Default)]
struct TypeScriptAliasIndex {
    by_directory: HashMap<String, TypeScriptAliasConfig>,
}

struct TypeScriptAliasConfig {
    base_path: String,
    explicit_base_url: Option<String>,
    mappings: Vec<TypeScriptPathMapping>,
    tsconfig: bool,
    fallback: bool,
}

struct TypeScriptPathMapping {
    pattern: String,
    substitutions: Vec<String>,
}

#[derive(Default)]
struct ResolutionIndex {
    javascript_members: javascript_member_resolution::JavascriptMemberIndex,
    javascript_frameworks: javascript_framework_resolution::JavascriptFrameworkIndex,
    candidates: CandidateMap,
    candidate_order: Vec<String>,
    default_exports: DefaultExportMap,
    parents: ParentMap,
    modules: ModulePathIndex,
    file_symbols: FileSymbolMap,
    file_ordinals: FileOrdinalMap,
    exports: FileExportMap,
    re_exports: Vec<ProjectReExport>,
    rust_named_re_exports: Vec<RustNamedReExport>,
    test_files: Vec<TestFileEvidence>,
    php: php_resolution::PhpResolutionIndex,
    javascript_exports: javascript_exports::ExportIndex,
    javascript_aliases: javascript_alias_exports::AliasIndex,
    salesforce: salesforce_resolution::SalesforceIndex,
    drupal_tags: drupal_tags::TagIndex,
    drupal_classes: drupal_resolution::ClassIndex,
    drupal_services: drupal_resolution::ServiceIndex,
    framework_methods: framework_methods::MethodIndex,
    generic: generic_resolution::GenericResolutionIndex,
    jvm: jvm_resolution::JvmResolutionIndex,
    types: qualified_member_resolution::TypeIndex,
    receivers: receiver_resolution::ReceiverIndex,
    qualtype: qualtype_resolution::TypeIndex,
    module_calls: module_call_resolution::ModuleCallIndex,
    rust_paths: rust_path_resolution::PathIndex,
    shell_sources: shell_resolution::SourceIndex,
    go_modules: go_module_paths::ModuleIndex,
    ocaml_modules: ocaml_module_resolution::ModuleIndex,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ResolutionReport {
    resolved: u64,
    unresolved: u64,
    diagnostics: u64,
    retained_bytes: u64,
    charged_high_water_bytes: u64,
}

struct ResolveBudget {
    charged_bytes: u64,
    maximum_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResolveGenerationFailure {
    reason: Option<PipelineFailureReason>,
}

impl ResolveGenerationFailure {
    const fn unclassified() -> Self {
        Self { reason: None }
    }

    const fn generation_capacity_exceeded() -> Self {
        Self {
            reason: Some(PipelineFailureReason::GenerationCapacityExceeded),
        }
    }

    const fn reason(self) -> Option<PipelineFailureReason> {
        self.reason
    }
}

impl ResolveBudget {
    fn new(initial_bytes: u64, maximum_bytes: u64) -> Result<Self, StageItemFailure> {
        if initial_bytes > maximum_bytes {
            return Err(StageItemFailure);
        }
        Ok(Self {
            charged_bytes: initial_bytes,
            maximum_bytes,
        })
    }

    fn charge(&mut self, bytes: u64) -> Result<(), StageItemFailure> {
        self.charged_bytes = self
            .charged_bytes
            .checked_add(bytes)
            .ok_or(StageItemFailure)?;
        if self.charged_bytes > self.maximum_bytes {
            Err(StageItemFailure)
        } else {
            Ok(())
        }
    }
}

struct ResolvedTarget {
    symbol_id: SymbolId,
    kind: SymbolKind,
    confidence: f32,
    provenance: &'static str,
}

struct ReferenceResolution {
    target: Option<ResolvedTarget>,
    unresolved_provenance: &'static str,
}

impl ReferenceResolution {
    fn resolved(target: ResolvedTarget) -> Self {
        Self {
            target: Some(target),
            unresolved_provenance: UNRESOLVED_PROVENANCE,
        }
    }

    const fn unresolved(provenance: &'static str) -> Self {
        Self {
            target: None,
            unresolved_provenance: provenance,
        }
    }
}

#[derive(Clone, Copy)]
enum EmbeddedSqlOperation {
    Read,
    Write,
    Ddl,
}

impl EmbeddedSqlOperation {
    const fn resolved_provenance(self) -> &'static str {
        match self {
            Self::Read => EMBEDDED_SQL_READ_PROVENANCE,
            Self::Write => EMBEDDED_SQL_WRITE_PROVENANCE,
            Self::Ddl => EMBEDDED_SQL_DDL_PROVENANCE,
        }
    }

    const fn unresolved_provenance(self) -> &'static str {
        match self {
            Self::Read => EMBEDDED_SQL_READ_UNRESOLVED,
            Self::Write => EMBEDDED_SQL_WRITE_UNRESOLVED,
            Self::Ddl => EMBEDDED_SQL_DDL_UNRESOLVED,
        }
    }
}

#[derive(Clone, Copy)]
struct EmbeddedSqlLookup<'a> {
    operation: EmbeddedSqlOperation,
    table: &'a str,
}

fn embedded_sql_lookup(value: Option<&str>) -> Option<EmbeddedSqlLookup<'_>> {
    let payload = value?.strip_prefix(EMBEDDED_SQL_RESOLUTION_PREFIX)?;
    let (operation, table) = payload.split_once("::")?;
    if table.is_empty() {
        return None;
    }
    let operation = match operation {
        "read" => EmbeddedSqlOperation::Read,
        "write" => EmbeddedSqlOperation::Write,
        "ddl" => EmbeddedSqlOperation::Ddl,
        _ => return None,
    };
    Some(EmbeddedSqlLookup { operation, table })
}

struct GoContainerMethods<'candidate> {
    candidate: &'candidate ResolutionCandidate,
    methods: BTreeMap<&'candidate str, Option<&'candidate str>>,
}

type GoContainers<'candidate> = BTreeMap<SymbolId, GoContainerMethods<'candidate>>;
type GoOwnerLookup = BTreeMap<String, Option<SymbolId>>;

struct GoMethodAttachment<'context, 'index> {
    index: &'index ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    containers: &'context mut GoContainers<'index>,
    owner_lookup: &'context GoOwnerLookup,
    key: &'context str,
    candidate: &'index ResolutionCandidate,
}

struct GoStructuralEdges<'context, Cancel> {
    index: &'context ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    cancelled: &'context mut Cancel,
}

struct GoContainerCandidate<'context, 'index> {
    index: &'index ResolutionIndex,
    key: &'context str,
    candidate: &'index ResolutionCandidate,
    budget: &'context mut ResolveBudget,
    containers: &'context mut GoContainers<'index>,
    owner_lookup: &'context mut GoOwnerLookup,
}

struct GoMethodCollection<'context, 'index, Cancel> {
    index: &'index ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    containers: &'context mut GoContainers<'index>,
    owner_lookup: &'context GoOwnerLookup,
    cancelled: &'context mut Cancel,
}

#[derive(Clone, Copy)]
struct GoOwnerQuery<'context, 'index> {
    file: &'context ResolutionFileContext,
    receiver_name: &'context str,
    parent_symbol_id: Option<&'context SymbolId>,
    containers: &'context GoContainers<'index>,
    owner_lookup: &'context GoOwnerLookup,
}

struct GoImplementationCollection<'context, 'index, Cancel> {
    index: &'index ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    containers: &'context GoContainers<'index>,
    cancelled: &'context mut Cancel,
}

struct GoStructImplementation<'context, 'index> {
    index: &'index ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    containers: &'context GoContainers<'index>,
    interface: &'context GoContainerMethods<'index>,
    struct_id: &'context SymbolId,
}

#[derive(Clone, Copy)]
struct GoMethodScopeQuery<'context, 'index> {
    index: &'index ResolutionIndex,
    structure: &'context GoContainerMethods<'index>,
    interface: &'context GoContainerMethods<'index>,
    method_name: &'context str,
}

struct GoInterfaceImplementation<'context, 'index, 'method, Cancel> {
    index: &'context ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    containers: &'context GoContainers<'index>,
    method_to_structs: &'context BTreeMap<&'method str, Vec<&'method SymbolId>>,
    interface: &'context GoContainerMethods<'index>,
    cancelled: &'context mut Cancel,
}

fn append_go_structural_edges<Cancel>(
    input: GoStructuralEdges<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let GoStructuralEdges {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    let (mut containers, owner_lookup) = collect_go_containers(index, budget, cancelled)?;
    if containers.is_empty() {
        return Ok(());
    }
    attach_go_methods(GoMethodCollection {
        index,
        facts,
        budget,
        containers: &mut containers,
        owner_lookup: &owner_lookup,
        cancelled,
    })?;
    append_go_implementations(GoImplementationCollection {
        index,
        facts,
        budget,
        containers: &containers,
        cancelled,
    })
}

fn collect_go_containers<'index, Cancel>(
    index: &'index ResolutionIndex,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<(GoContainers<'index>, GoOwnerLookup), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut containers = GoContainers::new();
    let mut owner_lookup = GoOwnerLookup::new();
    for (key, candidates) in ordered_resolution_candidates(index) {
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            collect_go_container_candidate(GoContainerCandidate {
                index,
                key,
                candidate,
                budget,
                containers: &mut containers,
                owner_lookup: &mut owner_lookup,
            })?;
        }
    }
    Ok((containers, owner_lookup))
}

fn collect_go_container_candidate(
    input: GoContainerCandidate<'_, '_>,
) -> Result<(), StageItemFailure> {
    let GoContainerCandidate {
        index,
        key,
        candidate,
        budget,
        containers,
        owner_lookup,
    } = input;
    if !is_go_container_candidate(key, candidate) {
        return Ok(());
    }
    let Some(file) = index.modules.files.get(&candidate.file_id) else {
        return Err(StageItemFailure);
    };
    if file.language != SourceLanguage::Go.as_str() {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<GoContainerMethods<'_>>()))
            .saturating_add(usize_to_u64(candidate.symbol_id.as_str().len())),
    )?;
    containers.insert(
        candidate.symbol_id.clone(),
        GoContainerMethods {
            candidate,
            methods: BTreeMap::new(),
        },
    );
    let owner_key = go_owner_lookup_key(file, &candidate.qualified_name)?;
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(owner_key.capacity()))
            .saturating_add(usize_to_u64(candidate.symbol_id.as_str().len())),
    )?;
    match owner_lookup.entry(owner_key) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(Some(candidate.symbol_id.clone()));
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            entry.insert(None);
        }
    }
    Ok(())
}

fn is_go_container_candidate(key: &str, candidate: &ResolutionCandidate) -> bool {
    candidate.qualified_name == key
        && candidate.top_level
        && matches!(candidate.kind, SymbolKind::Struct | SymbolKind::Interface)
}

fn attach_go_methods<Cancel>(
    input: GoMethodCollection<'_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let GoMethodCollection {
        index,
        facts,
        budget,
        containers,
        owner_lookup,
        cancelled,
    } = input;
    for (key, candidates) in ordered_resolution_candidates(index) {
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            attach_go_method_candidate(GoMethodAttachment {
                index,
                facts,
                budget,
                containers,
                owner_lookup,
                key,
                candidate,
            })?;
        }
    }
    Ok(())
}

fn attach_go_method_candidate(input: GoMethodAttachment<'_, '_>) -> Result<(), StageItemFailure> {
    let GoMethodAttachment {
        index,
        facts,
        budget,
        containers,
        owner_lookup,
        key,
        candidate,
    } = input;
    if candidate.qualified_name != key || candidate.kind != SymbolKind::Method {
        return Ok(());
    }
    let Some(file) = index.modules.files.get(&candidate.file_id) else {
        return Err(StageItemFailure);
    };
    if file.language != SourceLanguage::Go.as_str() {
        return Ok(());
    }
    let Some((receiver_name, method_name)) = go_method_parts(candidate) else {
        return Ok(());
    };
    let Some(owner) = go_method_owner(GoOwnerQuery {
        file,
        receiver_name,
        parent_symbol_id: candidate.parent_symbol_id.as_ref(),
        containers,
        owner_lookup,
    })?
    else {
        return Ok(());
    };
    let Some(container) = containers.get_mut(&owner) else {
        return Err(StageItemFailure);
    };
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(method_name.len()))
            .saturating_add(usize_to_u64(candidate.signature.len())),
    )?;
    match container.methods.entry(method_name) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(Some(candidate.signature.as_str()));
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            entry.insert(None);
        }
    }
    if candidate.parent_symbol_id.as_ref() != Some(&owner) {
        append_derived_edge(
            facts,
            budget,
            DerivedEdgeInput {
                source_symbol_id: &owner,
                target_symbol_id: &candidate.symbol_id,
                kind: EdgeKind::Contains,
                confidence: EXTRACTED_EDGE_CONFIDENCE,
                provenance: GO_RECEIVER_OWNERSHIP_PROVENANCE,
            },
        )?;
    }
    Ok(())
}

fn go_method_parts(candidate: &ResolutionCandidate) -> Option<(&str, &str)> {
    let (receiver_name, method_name) = candidate.qualified_name.rsplit_once("::")?;
    (!receiver_name.is_empty() && !method_name.is_empty()).then_some((receiver_name, method_name))
}

fn go_method_owner(input: GoOwnerQuery<'_, '_>) -> Result<Option<SymbolId>, StageItemFailure> {
    let GoOwnerQuery {
        file,
        receiver_name,
        parent_symbol_id,
        containers,
        owner_lookup,
    } = input;
    if let Some(parent) = parent_symbol_id.filter(|parent| containers.contains_key(*parent)) {
        return Ok(Some(parent.clone()));
    }
    let key = go_owner_lookup_key(file, receiver_name)?;
    Ok(owner_lookup.get(&key).cloned().flatten())
}

fn append_go_implementations<Cancel>(
    input: GoImplementationCollection<'_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let GoImplementationCollection {
        index,
        facts,
        budget,
        containers,
        cancelled,
    } = input;
    let method_to_structs = collect_go_method_implementors(containers, budget)?;

    for interface in containers
        .values()
        .filter(|container| container.candidate.kind == SymbolKind::Interface)
    {
        append_go_interface_implementations(GoInterfaceImplementation {
            index,
            facts,
            budget,
            containers,
            method_to_structs: &method_to_structs,
            interface,
            cancelled,
        })?;
    }
    Ok(())
}

fn collect_go_method_implementors<'container>(
    containers: &'container GoContainers<'_>,
    budget: &mut ResolveBudget,
) -> Result<BTreeMap<&'container str, Vec<&'container SymbolId>>, StageItemFailure> {
    let mut method_to_structs = BTreeMap::<&str, Vec<&SymbolId>>::new();
    for (struct_id, container) in containers {
        if container.candidate.kind != SymbolKind::Struct {
            continue;
        }
        for (method_name, signature) in &container.methods {
            if signature.is_none() {
                continue;
            }
            let bucket = method_to_structs.entry(method_name).or_default();
            budget.charge(usize_to_u64(size_of::<&SymbolId>()))?;
            bucket.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
            bucket.push(struct_id);
        }
    }
    Ok(method_to_structs)
}

fn append_go_interface_implementations<Cancel>(
    input: GoInterfaceImplementation<'_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let GoInterfaceImplementation {
        index,
        facts,
        budget,
        containers,
        method_to_structs,
        interface,
        cancelled,
    } = input;
    if cancelled() {
        return Err(StageItemFailure);
    }
    if interface.methods.is_empty() || interface.methods.values().any(Option::is_none) {
        return Ok(());
    }
    let Some(candidates) = interface
        .methods
        .keys()
        .filter_map(|method| method_to_structs.get(method))
        .min_by_key(|entries| entries.len())
    else {
        return Ok(());
    };
    for struct_id in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        append_go_struct_implementation(GoStructImplementation {
            index,
            facts,
            budget,
            containers,
            interface,
            struct_id,
        })?;
    }
    Ok(())
}

fn append_go_struct_implementation(
    input: GoStructImplementation<'_, '_>,
) -> Result<(), StageItemFailure> {
    let GoStructImplementation {
        index,
        facts,
        budget,
        containers,
        interface,
        struct_id,
    } = input;
    let Some(structure) = containers.get(struct_id) else {
        return Err(StageItemFailure);
    };
    if !go_struct_satisfies_interface(index, structure, interface) {
        return Ok(());
    }
    append_derived_edge(
        facts,
        budget,
        DerivedEdgeInput {
            source_symbol_id: &structure.candidate.symbol_id,
            target_symbol_id: &interface.candidate.symbol_id,
            kind: EdgeKind::Implements,
            confidence: GO_STRUCTURAL_CONFIDENCE,
            provenance: GO_IMPLEMENTS_PROVENANCE,
        },
    )
}

fn go_owner_lookup_key(
    file: &ResolutionFileContext,
    receiver_name: &str,
) -> Result<String, StageItemFailure> {
    let package = file.package.as_deref().unwrap_or_default();
    let capacity = file
        .directory
        .len()
        .checked_add(package.len())
        .and_then(|value| value.checked_add(receiver_name.len()))
        .and_then(|value| value.checked_add(2))
        .ok_or(StageItemFailure)?;
    let mut key = String::new();
    key.try_reserve_exact(capacity)
        .map_err(|_| StageItemFailure)?;
    key.push_str(&file.directory);
    key.push('\0');
    key.push_str(package);
    key.push('\0');
    key.push_str(receiver_name);
    Ok(key)
}

fn go_struct_satisfies_interface(
    index: &ResolutionIndex,
    structure: &GoContainerMethods<'_>,
    interface: &GoContainerMethods<'_>,
) -> bool {
    interface.methods.iter().all(|(name, expected)| {
        let Some(expected) = expected else {
            return false;
        };
        let Some(actual) = structure.methods.get(name).and_then(|value| *value) else {
            return false;
        };
        go_method_scope_compatible(GoMethodScopeQuery {
            index,
            structure,
            interface,
            method_name: name,
        }) && go_signatures_compatible(actual, expected)
    })
}

fn go_method_scope_compatible(input: GoMethodScopeQuery<'_, '_>) -> bool {
    let GoMethodScopeQuery {
        index,
        structure,
        interface,
        method_name,
    } = input;
    if method_name.chars().next().is_some_and(char::is_uppercase) {
        return true;
    }
    let Some(struct_file) = index.modules.files.get(&structure.candidate.file_id) else {
        return false;
    };
    let Some(interface_file) = index.modules.files.get(&interface.candidate.file_id) else {
        return false;
    };
    struct_file.directory == interface_file.directory
        && struct_file.package.is_some()
        && struct_file.package == interface_file.package
}

fn go_signatures_compatible(actual: &str, expected: &str) -> bool {
    if actual == expected {
        return true;
    }
    match (
        canonical_go_signature(actual),
        canonical_go_signature(expected),
    ) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => false,
    }
}

fn canonical_go_signature(signature: &str) -> Option<String> {
    let signature = signature.trim();
    let close = matching_group_end(signature, 0, GroupDelimiters::PARENTHESES)?;
    let parameters = signature.get(1..close)?;
    let remainder = signature.get(close.saturating_add(1)..)?.trim();
    let returns = remainder.strip_prefix(':').map_or(remainder, str::trim);
    let parameter_types = canonical_go_parameter_list(parameters)?;
    let return_types = if returns.is_empty() {
        Vec::new()
    } else if returns.starts_with('(') {
        let end = matching_group_end(returns, 0, GroupDelimiters::PARENTHESES)?;
        if !returns.get(end.saturating_add(1)..)?.trim().is_empty() {
            return None;
        }
        canonical_go_parameter_list(returns.get(1..end)?)?
    } else {
        vec![normalize_go_type(returns)?]
    };
    let required = parameter_types
        .iter()
        .chain(&return_types)
        .try_fold(3_usize, |total, value| {
            total.checked_add(value.len().saturating_add(1))
        })?;
    let mut canonical = String::new();
    canonical.try_reserve_exact(required).ok()?;
    for parameter in parameter_types {
        canonical.push_str(&parameter);
        canonical.push(';');
    }
    canonical.push_str("->");
    for result in return_types {
        canonical.push_str(&result);
        canonical.push(';');
    }
    Some(canonical)
}

fn canonical_go_parameter_list(list: &str) -> Option<Vec<String>> {
    let segments = split_top_level(list, ',')?;
    let mut output = Vec::new();
    let mut pending_identifiers = Vec::new();
    for segment in segments {
        let segment = segment.trim();
        if segment.is_empty() {
            return None;
        }
        append_go_parameter_segment(segment, &mut pending_identifiers, &mut output)?;
    }
    flush_go_parameter_types(&mut pending_identifiers, &mut output)?;
    Some(output)
}

fn append_go_parameter_segment<'parameter>(
    segment: &'parameter str,
    pending_identifiers: &mut Vec<&'parameter str>,
    output: &mut Vec<String>,
) -> Option<()> {
    if let Some((name, value_type)) = split_go_named_parameter(segment) {
        if !go_identifier(name) {
            return None;
        }
        let value_type = normalize_go_type(value_type)?;
        output
            .try_reserve(pending_identifiers.len().saturating_add(1))
            .ok()?;
        for _ in pending_identifiers.drain(..) {
            output.push(value_type.clone());
        }
        output.push(value_type);
        return Some(());
    }
    if go_identifier(segment) {
        pending_identifiers.try_reserve_exact(1).ok()?;
        pending_identifiers.push(segment);
        return Some(());
    }
    flush_go_parameter_types(pending_identifiers, output)?;
    output.try_reserve_exact(1).ok()?;
    output.push(normalize_go_type(segment)?);
    Some(())
}

fn flush_go_parameter_types(
    pending_identifiers: &mut Vec<&str>,
    output: &mut Vec<String>,
) -> Option<()> {
    output.try_reserve(pending_identifiers.len()).ok()?;
    for identifier in pending_identifiers.drain(..) {
        output.push(normalize_go_type(identifier)?);
    }
    Some(())
}

fn split_go_named_parameter(value: &str) -> Option<(&str, &str)> {
    let mut depths = [0_u16; 3];
    for (index, character) in value.char_indices() {
        update_group_depths(&mut depths, character)?;
        if depths == [0, 0, 0] && character.is_whitespace() {
            let name = value.get(..index)?.trim();
            let value_type = value.get(index..)?.trim();
            if go_identifier(name) && !value_type.is_empty() {
                return Some((name, value_type));
            }
        }
    }
    None
}

fn split_top_level(value: &str, delimiter: char) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut depths = [0_u16; 3];
    let mut start = 0_usize;
    for (index, character) in value.char_indices() {
        if character == delimiter && depths == [0, 0, 0] {
            segments.try_reserve_exact(1).ok()?;
            segments.push(value.get(start..index)?);
            start = index.checked_add(character.len_utf8())?;
            continue;
        }
        update_group_depths(&mut depths, character)?;
    }
    if depths != [0, 0, 0] {
        return None;
    }
    if !value.trim().is_empty() {
        segments.try_reserve_exact(1).ok()?;
        segments.push(value.get(start..)?);
    }
    Some(segments)
}

fn update_group_depths(depths: &mut [u16; 3], character: char) -> Option<()> {
    let (slot, opening) = match character {
        '(' => (Some(0), true),
        ')' => (Some(0), false),
        '[' => (Some(1), true),
        ']' => (Some(1), false),
        '{' => (Some(2), true),
        '}' => (Some(2), false),
        _ => (None, false),
    };
    let Some(slot) = slot else {
        return Some(());
    };
    if opening {
        depths[slot] = depths[slot].checked_add(1)?;
    } else {
        depths[slot] = depths[slot].checked_sub(1)?;
    }
    Some(())
}

#[derive(Clone, Copy)]
struct GroupDelimiters {
    open: char,
    close: char,
}

impl GroupDelimiters {
    const PARENTHESES: Self = Self {
        open: '(',
        close: ')',
    };
}

fn matching_group_end(value: &str, start: usize, delimiters: GroupDelimiters) -> Option<usize> {
    if value.get(start..)?.chars().next()? != delimiters.open {
        return None;
    }
    let mut depth = 0_u16;
    for (relative, character) in value.get(start..)?.char_indices() {
        if character == delimiters.open {
            depth = depth.checked_add(1)?;
        } else if character == delimiters.close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return start.checked_add(relative);
            }
        }
    }
    None
}

fn normalize_go_type(value: &str) -> Option<String> {
    let mut normalized = String::new();
    normalized.try_reserve_exact(value.len()).ok()?;
    for character in value.chars().filter(|character| !character.is_whitespace()) {
        normalized.push(character);
    }
    (!normalized.is_empty()).then_some(normalized)
}

fn go_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

type VisibleExportMap = BTreeMap<String, Option<SymbolId>>;

struct ResolutionMutation<'context, Cancel> {
    index: &'context ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    cancelled: &'context mut Cancel,
}

struct VisibleExportQuery<'context, Cancel> {
    index: &'context ResolutionIndex,
    file_id: &'context FileId,
    include_default: bool,
    stack: &'context mut BTreeSet<FileId>,
    cancelled: &'context mut Cancel,
}

#[derive(Clone, Copy)]
struct TestEdgeInput<'context> {
    source_symbol_id: &'context SymbolId,
    target_symbol_id: &'context SymbolId,
    confidence: f32,
    provenance: &'static str,
}

struct ConventionalTestSubjectQuery<'context, Cancel> {
    index: &'context ResolutionIndex,
    source: &'context ResolutionFileContext,
    language: SourceLanguage,
    cancelled: &'context mut Cancel,
}

struct ImportedTestSubjectQuery<'context, Cancel> {
    index: &'context ResolutionIndex,
    evidence: &'context TestFileEvidence,
    source: &'context ResolutionFileContext,
    cancelled: &'context mut Cancel,
}

struct SubjectMatchInput<'context> {
    index: &'context ResolutionIndex,
    directory: &'context str,
    subject: &'context str,
    extensions: &'context [&'context str],
    test_path: &'context str,
    subjects: &'context mut BTreeSet<FileId>,
}

struct SubjectAdmission<'context> {
    index: &'context ResolutionIndex,
    candidate: &'context str,
    test_path: &'context str,
    subjects: &'context mut BTreeSet<FileId>,
}

fn append_module_reexport_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    if index.re_exports.is_empty() {
        return Ok(());
    }
    let mut wildcard_sources = BTreeSet::new();
    for re_export in &index.re_exports {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !re_export.namespace {
            wildcard_sources.insert(re_export.source_file_id.clone());
            continue;
        }
        append_namespace_reexport_edges(
            ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            re_export,
        )?;
    }

    for source_file in wildcard_sources {
        append_wildcard_reexport_edges(
            ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            &source_file,
        )?;
    }
    Ok(())
}

fn append_namespace_reexport_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
    re_export: &ProjectReExport,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    let Some(target_file) = resolve_reexport_target(index, re_export) else {
        return Ok(());
    };
    let mut stack = BTreeSet::new();
    let visible = collect_visible_exports(VisibleExportQuery {
        index,
        file_id: target_file,
        include_default: true,
        stack: &mut stack,
        cancelled,
    })?;
    for (name, target) in &visible {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(target) = target else {
            continue;
        };
        let Some((confidence, provenance)) = javascript_exports::edge_evidence(
            javascript_exports::ExportEdgeQuery {
                index,
                file: target_file,
                name,
                target,
                namespace: Some(re_export),
            },
            cancelled,
        )?
        else {
            continue;
        };
        append_derived_edge(
            facts,
            budget,
            DerivedEdgeInput {
                source_symbol_id: &re_export.source_symbol_id,
                target_symbol_id: target,
                kind: EdgeKind::Exports,
                confidence,
                provenance,
            },
        )?;
    }
    Ok(())
}

fn append_wildcard_reexport_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
    source_file: &FileId,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    if cancelled() {
        return Err(StageItemFailure);
    }
    let visible = collect_wildcard_exports(index, source_file, cancelled)?;
    let source_symbol = index
        .file_symbols
        .get(source_file)
        .ok_or(StageItemFailure)?;
    for (name, target) in &visible {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(target) = target else {
            continue;
        };
        let Some((confidence, provenance)) = javascript_exports::edge_evidence(
            javascript_exports::ExportEdgeQuery {
                index,
                file: source_file,
                name,
                target,
                namespace: None,
            },
            cancelled,
        )?
        else {
            continue;
        };
        append_derived_edge(
            facts,
            budget,
            DerivedEdgeInput {
                source_symbol_id: source_symbol,
                target_symbol_id: target,
                kind: EdgeKind::Exports,
                confidence,
                provenance,
            },
        )?;
    }
    Ok(())
}

fn collect_visible_exports<Cancel>(
    input: VisibleExportQuery<'_, Cancel>,
) -> Result<VisibleExportMap, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let VisibleExportQuery {
        index,
        file_id,
        include_default,
        stack,
        cancelled,
    } = input;
    if !stack.insert(file_id.clone()) {
        return Ok(VisibleExportMap::new());
    }
    let result = collect_visible_exports_inner(VisibleExportQuery {
        index,
        file_id,
        include_default,
        stack,
        cancelled,
    });
    stack.remove(file_id);
    result
}

fn collect_visible_exports_inner<Cancel>(
    input: VisibleExportQuery<'_, Cancel>,
) -> Result<VisibleExportMap, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let VisibleExportQuery {
        index,
        file_id,
        include_default,
        stack,
        cancelled,
    } = input;
    let mut visible = VisibleExportMap::new();
    let mut explicit_names = BTreeSet::new();
    collect_explicit_visible_exports(ExplicitVisibleExports {
        index,
        file_id,
        include_default,
        visible: &mut visible,
        explicit_names: &mut explicit_names,
        cancelled,
    })?;
    collect_nested_visible_exports(NestedVisibleExports {
        index,
        file_id,
        stack,
        cancelled,
        explicit_names: &explicit_names,
        visible: &mut visible,
    })?;
    Ok(visible)
}

struct ExplicitVisibleExports<'context, Cancel> {
    index: &'context ResolutionIndex,
    file_id: &'context FileId,
    include_default: bool,
    visible: &'context mut VisibleExportMap,
    explicit_names: &'context mut BTreeSet<String>,
    cancelled: &'context mut Cancel,
}

fn collect_explicit_visible_exports<Cancel>(
    input: ExplicitVisibleExports<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ExplicitVisibleExports {
        index,
        file_id,
        include_default,
        visible,
        explicit_names,
        cancelled,
    } = input;
    if let Some(exports) = index.exports.get(file_id) {
        for (name, target) in exports {
            if cancelled() {
                return Err(StageItemFailure);
            }
            explicit_names.insert(try_clone_text(name)?);
            if !include_default
                && (name == "default"
                    || target.as_ref().is_some_and(|export| export.default_export))
            {
                continue;
            }
            visible.insert(
                try_clone_text(name)?,
                target.as_ref().map(|export| export.symbol_id.clone()),
            );
        }
    }
    Ok(())
}

struct NestedVisibleExports<'context, Cancel> {
    index: &'context ResolutionIndex,
    file_id: &'context FileId,
    stack: &'context mut BTreeSet<FileId>,
    cancelled: &'context mut Cancel,
    explicit_names: &'context BTreeSet<String>,
    visible: &'context mut VisibleExportMap,
}

fn collect_nested_visible_exports<Cancel>(
    input: NestedVisibleExports<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let NestedVisibleExports {
        index,
        file_id,
        stack,
        cancelled,
        explicit_names,
        visible,
    } = input;
    for re_export in index
        .re_exports
        .iter()
        .filter(|entry| !entry.namespace && &entry.source_file_id == file_id)
    {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(target_file) = resolve_reexport_target(index, re_export) else {
            continue;
        };
        let nested = collect_visible_exports(VisibleExportQuery {
            index,
            file_id: target_file,
            include_default: false,
            stack,
            cancelled,
        })?;
        for (name, target) in nested {
            if explicit_names.contains(&name) {
                continue;
            }
            merge_visible_export(visible, name, target);
        }
    }
    Ok(())
}

fn collect_wildcard_exports<Cancel>(
    index: &ResolutionIndex,
    source_file: &FileId,
    cancelled: &mut Cancel,
) -> Result<VisibleExportMap, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let explicit_names = index
        .exports
        .get(source_file)
        .map(|exports| exports.keys().map(String::as_str).collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let mut visible = VisibleExportMap::new();
    for re_export in index
        .re_exports
        .iter()
        .filter(|entry| !entry.namespace && &entry.source_file_id == source_file)
    {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(target_file) = resolve_reexport_target(index, re_export) else {
            continue;
        };
        let mut stack = BTreeSet::from([source_file.clone()]);
        let nested = collect_visible_exports(VisibleExportQuery {
            index,
            file_id: target_file,
            include_default: false,
            stack: &mut stack,
            cancelled,
        })?;
        for (name, target) in nested {
            if explicit_names.contains(name.as_str()) {
                continue;
            }
            merge_visible_export(&mut visible, name, target);
        }
    }
    Ok(visible)
}

fn merge_visible_export(visible: &mut VisibleExportMap, name: String, target: Option<SymbolId>) {
    if let Some(existing) = visible.get_mut(&name) {
        if existing.as_ref() != target.as_ref() {
            *existing = None;
        }
    } else {
        visible.insert(name, target);
    }
}

fn resolve_reexport_target<'index>(
    index: &'index ResolutionIndex,
    re_export: &ProjectReExport,
) -> Option<&'index FileId> {
    let source = index.modules.files.get(&re_export.source_file_id)?;
    resolve_module_file(
        &index.modules,
        ModuleResolutionRequest {
            importing_path: &source.path,
            specifier: &re_export.module_specifier,
            importing_language: &source.language,
        },
    )
}

fn append_test_subject_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    for evidence in &index.test_files {
        if cancelled() {
            return Err(StageItemFailure);
        }
        append_test_subject_edge_set(
            ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            evidence,
        )?;
    }
    Ok(())
}

fn append_test_subject_edge_set<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
    evidence: &TestFileEvidence,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let source = input
        .index
        .modules
        .files
        .get(&evidence.file_id)
        .ok_or(StageItemFailure)?;
    let source_symbol = input
        .index
        .file_symbols
        .get(&evidence.file_id)
        .ok_or(StageItemFailure)?;
    if append_rust_test_subject(RustTestSubjectInput {
        index: input.index,
        facts: &mut *input.facts,
        budget: &mut *input.budget,
        source,
        source_symbol,
    })? {
        return Ok(());
    }
    if evidence.has_inline_tests && !is_test_source_path(&source.path) {
        return append_test_edge(
            input.facts,
            input.budget,
            TestEdgeInput {
                source_symbol_id: source_symbol,
                target_symbol_id: source_symbol,
                confidence: EXTRACTED_EDGE_CONFIDENCE,
                provenance: RUST_INLINE_TEST_PROVENANCE,
            },
        );
    }
    append_conventional_test_subjects(ConventionalTestEdgeInput {
        mutation: input,
        evidence,
        source,
        source_symbol,
    })
}

struct RustTestSubjectInput<'a> {
    index: &'a ResolutionIndex,
    facts: &'a mut GenerationFacts,
    budget: &'a mut ResolveBudget,
    source: &'a ResolutionFileContext,
    source_symbol: &'a SymbolId,
}

fn append_rust_test_subject(input: RustTestSubjectInput<'_>) -> Result<bool, StageItemFailure> {
    let RustTestSubjectInput {
        index,
        facts,
        budget,
        source,
        source_symbol,
    } = input;
    if source.language != SourceLanguage::Rust.as_str() {
        return Ok(false);
    }
    let Some(crate_root) = rust_integration_crate_root(&source.path)? else {
        return Ok(false);
    };
    let Some(target) = rust_crate_entry(index, &crate_root) else {
        return Ok(true);
    };
    let target_symbol = index.file_symbols.get(target).ok_or(StageItemFailure)?;
    append_test_edge(
        facts,
        budget,
        TestEdgeInput {
            source_symbol_id: source_symbol,
            target_symbol_id: target_symbol,
            confidence: EXTRACTED_EDGE_CONFIDENCE,
            provenance: RUST_INTEGRATION_TEST_PROVENANCE,
        },
    )?;
    Ok(true)
}

struct ConventionalTestEdgeInput<'context, Cancel> {
    mutation: ResolutionMutation<'context, Cancel>,
    evidence: &'context TestFileEvidence,
    source: &'context ResolutionFileContext,
    source_symbol: &'context SymbolId,
}

fn append_conventional_test_subjects<Cancel>(
    input: ConventionalTestEdgeInput<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ConventionalTestEdgeInput {
        mutation,
        evidence,
        source,
        source_symbol,
    } = input;
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = mutation;
    let Some(language) = SourceLanguage::from_stable_str(&source.language) else {
        return Err(StageItemFailure);
    };
    let mut subjects = conventional_test_subjects(ConventionalTestSubjectQuery {
        index,
        source,
        language,
        cancelled,
    })?;
    let (confidence, provenance) = if subjects.is_empty() {
        subjects = imported_test_subjects(ImportedTestSubjectQuery {
            index,
            evidence,
            source,
            cancelled,
        })?;
        (TEST_IMPORT_CONFIDENCE, TEST_IMPORT_PROVENANCE)
    } else {
        (TEST_CONVENTION_CONFIDENCE, TEST_CONVENTION_PROVENANCE)
    };
    for subject in subjects {
        let target_symbol = index.file_symbols.get(&subject).ok_or(StageItemFailure)?;
        append_test_edge(
            facts,
            budget,
            TestEdgeInput {
                source_symbol_id: source_symbol,
                target_symbol_id: target_symbol,
                confidence,
                provenance,
            },
        )?;
    }
    Ok(())
}

fn append_test_edge(
    facts: &mut GenerationFacts,
    budget: &mut ResolveBudget,
    input: TestEdgeInput<'_>,
) -> Result<(), StageItemFailure> {
    let TestEdgeInput {
        source_symbol_id,
        target_symbol_id,
        confidence,
        provenance,
    } = input;
    let retained = usize_to_u64(size_of::<EdgeInput>())
        .saturating_add(usize_to_u64(source_symbol_id.as_str().len()))
        .saturating_add(usize_to_u64(target_symbol_id.as_str().len()))
        .saturating_add(usize_to_u64(provenance.len()));
    budget.charge(retained)?;
    facts
        .edges
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    facts.edges.push(EdgeInput {
        source_symbol_id: source_symbol_id.clone(),
        target_symbol_id: target_symbol_id.clone(),
        kind: EdgeKind::Tests,
        confidence,
        provenance: try_clone_text(provenance)?,
        site_count: 1,
    });
    Ok(())
}

fn conventional_test_subjects<Cancel>(
    input: ConventionalTestSubjectQuery<'_, Cancel>,
) -> Result<BTreeSet<FileId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ConventionalTestSubjectQuery {
        index,
        source,
        language,
        cancelled,
    } = input;
    let Some(subject) = test_subject_basename(&source.path, language) else {
        return Ok(BTreeSet::new());
    };
    let extensions = test_subject_extensions(&source.path, language);
    let mut subjects = BTreeSet::new();
    add_subject_matches(SubjectMatchInput {
        index,
        directory: &source.directory,
        subject,
        extensions,
        test_path: &source.path,
        subjects: &mut subjects,
    })?;
    for root in mirrored_test_roots(&source.directory)? {
        if cancelled() {
            return Err(StageItemFailure);
        }
        add_subject_matches(SubjectMatchInput {
            index,
            directory: &root,
            subject,
            extensions,
            test_path: &source.path,
            subjects: &mut subjects,
        })?;
    }
    if !subjects.is_empty() {
        return Ok(subjects);
    }

    let mut best_score = None;
    let mut best = None;
    let mut tied = false;
    for (file_id, candidate) in &index.modules.files {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.path == source.path
            || !plausible_test_subject(&candidate.path)
            || !path_has_subject(&candidate.path, subject, extensions)
        {
            continue;
        }
        let score = common_path_prefix_segments(&source.directory, &candidate.directory);
        match best_score {
            None => {
                best_score = Some(score);
                best = Some(file_id);
                tied = false;
            }
            Some(current) if score > current => {
                best_score = Some(score);
                best = Some(file_id);
                tied = false;
            }
            Some(current) if score == current => tied = true,
            Some(_) => {}
        }
    }
    if !tied && let Some(file_id) = best {
        subjects.insert(file_id.clone());
    }
    Ok(subjects)
}

fn imported_test_subjects<Cancel>(
    input: ImportedTestSubjectQuery<'_, Cancel>,
) -> Result<BTreeSet<FileId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ImportedTestSubjectQuery {
        index,
        evidence,
        source,
        cancelled,
    } = input;
    let mut subjects = BTreeSet::new();
    for specifier in &evidence.import_specifiers {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let target = resolve_module_file(
            &index.modules,
            ModuleResolutionRequest {
                importing_path: &source.path,
                specifier,
                importing_language: &source.language,
            },
        )
        .or_else(|| resolve_package_test_import(index, &source.language, specifier));
        let Some(target) = target else {
            continue;
        };
        let Some(target_file) = index.modules.files.get(target) else {
            return Err(StageItemFailure);
        };
        if target != &evidence.file_id && plausible_test_subject(&target_file.path) {
            subjects.insert(target.clone());
        }
    }
    Ok(subjects)
}

fn resolve_package_test_import<'index>(
    index: &'index ResolutionIndex,
    source_language: &str,
    specifier: &str,
) -> Option<&'index FileId> {
    if !matches!(source_language, "java" | "kotlin" | "scala")
        || specifier.starts_with('.')
        || !specifier.contains('.')
    {
        return None;
    }
    for candidate in [
        Some(specifier),
        specifier.rsplit_once('.').map(|(owner, _)| owner),
    ] {
        let candidate = candidate?.trim_end_matches(".*");
        let suffix = package_test_suffix(candidate)?;
        match unique_package_test_file(index, &suffix) {
            PackageTestFileMatch::Unique(file_id) => return Some(file_id),
            PackageTestFileMatch::Absent => {}
            PackageTestFileMatch::Ambiguous => return None,
        }
    }
    None
}

fn package_test_suffix(candidate: &str) -> Option<String> {
    let mut suffix = String::new();
    suffix
        .try_reserve_exact(candidate.len().saturating_add(7))
        .ok()?;
    suffix.push('/');
    suffix.extend(
        candidate
            .chars()
            .map(|character| if character == '.' { '/' } else { character }),
    );
    Some(suffix)
}

enum PackageTestFileMatch<'index> {
    Absent,
    Unique(&'index FileId),
    Ambiguous,
}

fn unique_package_test_file<'index>(
    index: &'index ResolutionIndex,
    suffix: &str,
) -> PackageTestFileMatch<'index> {
    let mut matched = None;
    for (file_id, file) in &index.modules.files {
        if !matches!(file.language.as_str(), "java" | "kotlin" | "scala")
            || ![".kt", ".kts", ".java", ".scala"]
                .iter()
                .any(|extension| file.path.ends_with(&format!("{suffix}{extension}")))
        {
            continue;
        }
        if matched.is_some() {
            return PackageTestFileMatch::Ambiguous;
        }
        matched = Some(file_id);
    }
    matched.map_or(PackageTestFileMatch::Absent, PackageTestFileMatch::Unique)
}

fn rust_integration_crate_root(path: &str) -> Result<Option<String>, StageItemFailure> {
    if !std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
    {
        return Ok(None);
    }
    let components = path.split('/').collect::<Vec<_>>();
    let Some(test_index) = components
        .iter()
        .position(|component| component.eq_ignore_ascii_case("tests"))
    else {
        return Ok(None);
    };
    if components.len() != test_index.saturating_add(2) {
        return Ok(None);
    }
    let mut root = String::new();
    let required = components[..test_index]
        .iter()
        .try_fold(0_usize, |total, component| {
            total.checked_add(component.len().saturating_add(1))
        })
        .ok_or(StageItemFailure)?;
    root.try_reserve_exact(required)
        .map_err(|_| StageItemFailure)?;
    for component in &components[..test_index] {
        root.push_str(component);
        root.push('/');
    }
    Ok(Some(root))
}

fn rust_crate_entry<'index>(index: &'index ResolutionIndex, root: &str) -> Option<&'index FileId> {
    for relative in ["src/lib.rs", "src/main.rs"] {
        let path = joined_path(root.trim_end_matches('/'), relative).ok()?;
        if let Some(file_id) = unique_file_for_path(index, &path) {
            return Some(file_id);
        }
    }
    None
}

fn test_subject_basename(path: &str, language: SourceLanguage) -> Option<&str> {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let subject = match language {
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => strip_suffix_ignore_ascii_case(stem, ".test")
            .or_else(|| strip_suffix_ignore_ascii_case(stem, ".spec")),
        SourceLanguage::Python | SourceLanguage::Rust => stem
            .strip_prefix("test_")
            .or_else(|| stem.strip_suffix("_test")),
        SourceLanguage::Go => stem.strip_suffix("_test"),
        SourceLanguage::Ruby => stem
            .strip_suffix("_spec")
            .or_else(|| stem.strip_suffix("_test")),
        SourceLanguage::Java
        | SourceLanguage::Kotlin
        | SourceLanguage::Scala
        | SourceLanguage::CSharp
        | SourceLanguage::Swift => stem
            .strip_suffix("Tests")
            .or_else(|| stem.strip_suffix("Test"))
            .or_else(|| {
                matches!(
                    language,
                    SourceLanguage::Kotlin | SourceLanguage::Scala | SourceLanguage::Swift
                )
                .then(|| stem.strip_suffix("Spec"))
                .flatten()
            }),
        _ => None,
    }?;
    (!subject.is_empty()).then_some(subject)
}

fn strip_suffix_ignore_ascii_case<'value>(value: &'value str, suffix: &str) -> Option<&'value str> {
    let start = value.len().checked_sub(suffix.len())?;
    let candidate = value.get(start..)?;
    candidate
        .eq_ignore_ascii_case(suffix)
        .then(|| value.get(..start))
        .flatten()
}

fn test_subject_extensions(path: &str, language: SourceLanguage) -> &'static [&'static str] {
    let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
    if language == SourceLanguage::TypeScript {
        return typescript_subject_extensions(extension);
    }
    if language == SourceLanguage::JavaScript {
        return javascript_subject_extensions(extension);
    }
    TEST_SUBJECT_EXTENSIONS
        .iter()
        .find_map(|(candidate, extensions)| (*candidate == language).then_some(*extensions))
        .unwrap_or_default()
}

fn typescript_subject_extensions(extension: &str) -> &'static [&'static str] {
    if extension.eq_ignore_ascii_case("mts") {
        &["mts", "ts"]
    } else if extension.eq_ignore_ascii_case("cts") {
        &["cts", "ts"]
    } else {
        &["ts"]
    }
}

fn javascript_subject_extensions(extension: &str) -> &'static [&'static str] {
    if extension.eq_ignore_ascii_case("mjs") {
        &["mjs", "js"]
    } else if extension.eq_ignore_ascii_case("cjs") {
        &["cjs", "js"]
    } else {
        &["js"]
    }
}

const TEST_SUBJECT_EXTENSIONS: &[(SourceLanguage, &[&str])] = &[
    (SourceLanguage::Tsx, &["tsx", "ts"]),
    (SourceLanguage::Jsx, &["jsx", "js"]),
    (SourceLanguage::Python, &["py", "pyi"]),
    (SourceLanguage::Rust, &["rs"]),
    (SourceLanguage::Go, &["go"]),
    (SourceLanguage::Ruby, &["rb"]),
    (SourceLanguage::Java, &["java"]),
    (SourceLanguage::Kotlin, &["kt", "kts"]),
    (SourceLanguage::Scala, &["scala"]),
    (SourceLanguage::CSharp, &["cs"]),
    (SourceLanguage::Swift, &["swift"]),
];

fn add_subject_matches(input: SubjectMatchInput<'_>) -> Result<(), StageItemFailure> {
    let SubjectMatchInput {
        index,
        directory,
        subject,
        extensions,
        test_path,
        subjects,
    } = input;
    for extension in extensions {
        let direct_name = suffixed_name(subject, ".", extension)?;
        let direct = joined_path(directory, &direct_name)?;
        admit_subject_path(SubjectAdmission {
            index,
            candidate: &direct,
            test_path,
            subjects,
        });
        let index_name = suffixed_name("index", ".", extension)?;
        let subject_directory = joined_path(directory, subject)?;
        let indexed = joined_path(&subject_directory, &index_name)?;
        admit_subject_path(SubjectAdmission {
            index,
            candidate: &indexed,
            test_path,
            subjects,
        });
    }
    Ok(())
}

fn admit_subject_path(input: SubjectAdmission<'_>) {
    let SubjectAdmission {
        index,
        candidate,
        test_path,
        subjects,
    } = input;
    if candidate != test_path
        && plausible_test_subject(candidate)
        && let Some(file_id) = unique_file_for_path(index, candidate)
    {
        subjects.insert(file_id.clone());
    }
}

fn unique_file_for_path<'index>(
    index: &'index ResolutionIndex,
    path: &str,
) -> Option<&'index FileId> {
    let files = index.modules.exact.get(path)?;
    (files.len() == 1).then(|| files.first()).flatten()
}

fn mirrored_test_roots(directory: &str) -> Result<Vec<String>, StageItemFailure> {
    let retained = directory
        .split('/')
        .filter(|component| {
            !matches_ignore_ascii_case(
                component,
                &["test", "tests", "__tests__", "spec", "specs", "__specs__"],
            )
        })
        .collect::<Vec<_>>();
    let mut roots = Vec::new();
    if retained.is_empty() {
        roots.try_reserve_exact(5).map_err(|_| StageItemFailure)?;
        for root in ["", "src", "lib", "app", "packages"] {
            roots.push(try_clone_text(root)?);
        }
    } else {
        roots.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
        roots.push(join_segments(&retained)?);
    }
    Ok(roots)
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn join_segments(segments: &[&str]) -> Result<String, StageItemFailure> {
    let required = segments
        .iter()
        .try_fold(0_usize, |total, segment| {
            total.checked_add(segment.len().saturating_add(usize::from(total > 0)))
        })
        .ok_or(StageItemFailure)?;
    let mut joined = String::new();
    joined
        .try_reserve_exact(required)
        .map_err(|_| StageItemFailure)?;
    for segment in segments {
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(segment);
    }
    Ok(joined)
}

fn joined_path(directory: &str, name: &str) -> Result<String, StageItemFailure> {
    let directory = directory.trim_matches('/');
    let separator = usize::from(!directory.is_empty() && !name.is_empty());
    let required = directory
        .len()
        .checked_add(name.len())
        .and_then(|value| value.checked_add(separator))
        .ok_or(StageItemFailure)?;
    let mut path = String::new();
    path.try_reserve_exact(required)
        .map_err(|_| StageItemFailure)?;
    path.push_str(directory);
    if separator > 0 {
        path.push('/');
    }
    path.push_str(name.trim_start_matches('/'));
    Ok(path)
}

fn suffixed_name(prefix: &str, separator: &str, suffix: &str) -> Result<String, StageItemFailure> {
    let required = prefix
        .len()
        .checked_add(separator.len())
        .and_then(|value| value.checked_add(suffix.len()))
        .ok_or(StageItemFailure)?;
    let mut value = String::new();
    value
        .try_reserve_exact(required)
        .map_err(|_| StageItemFailure)?;
    value.push_str(prefix);
    value.push_str(separator);
    value.push_str(suffix);
    Ok(value)
}

fn plausible_test_subject(path: &str) -> bool {
    !path.split('/').any(|component| {
        matches_ignore_ascii_case(
            component,
            &[
                "test",
                "tests",
                "__tests__",
                "__mocks__",
                "spec",
                "fixtures",
                "fixture",
                "test-bed",
                "test-beds",
            ],
        )
    }) && !is_test_source_path(path)
}

fn path_has_subject(path: &str, subject: &str, extensions: &[&str]) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    extensions.iter().any(|extension| {
        filename
            .strip_suffix(extension)
            .and_then(|stem| stem.strip_suffix('.'))
            == Some(subject)
    })
}

fn common_path_prefix_segments(left: &str, right: &str) -> usize {
    left.split('/')
        .zip(right.split('/'))
        .take_while(|(left, right)| left == right)
        .count()
}

struct ResolveGenerationRequest {
    extracted: NativeFactAccumulator,
    maximum_bytes: u64,
    source_root: SourceRoot,
    evidence_policy: NativeEvidencePolicy,
    clone_policy: NativeClonePolicy,
}

struct CloneAnalysisInput<'context> {
    extracted: &'context mut NativeFactAccumulator,
    source_root: &'context SourceRoot,
    policy: NativeClonePolicy,
    budget: &'context mut ResolveBudget,
}

struct CloneCandidateCollectionInput<'context> {
    extracted: &'context mut NativeFactAccumulator,
    source_root: &'context SourceRoot,
    budget: &'context mut ResolveBudget,
}

struct PartialCloneBand<'context, Cancel> {
    extracted: &'context NativeFactAccumulator,
    candidates: &'context mut [CloneAnalysisCandidate],
    minimum_overlap_ppm: u32,
    exclude_first_band: bool,
    cancelled: &'context mut Cancel,
}

struct PartialCloneLink<'context> {
    candidates: &'context mut [CloneAnalysisCandidate],
    left: usize,
    right: usize,
    overlap_ppm: u32,
    minimum_overlap_ppm: u32,
}

struct PersistPartialClones<'context, Cancel> {
    extracted: &'context mut NativeFactAccumulator,
    candidates: &'context [CloneAnalysisCandidate],
    budget: &'context mut ResolveBudget,
    cancelled: &'context mut Cancel,
}

struct DerivedResolutionEvidence<'context, Cancel> {
    index: &'context ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    policy: NativeEvidencePolicy,
    cancelled: &'context mut Cancel,
}

fn resolve_generation<Cancel>(
    request: ResolveGenerationRequest,
    mut cancelled: Cancel,
) -> Result<(GenerationFacts, ResolutionReport), ResolveGenerationFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolveGenerationRequest {
        mut extracted,
        maximum_bytes,
        source_root,
        evidence_policy,
        clone_policy,
    } = request;
    if cancelled() {
        return Err(ResolveGenerationFailure::unclassified());
    }
    let working_limit = maximum_bytes
        .checked_mul(RESOLVE_WORKING_MULTIPLIER)
        .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
    let mut budget = ResolveBudget::new(extracted.retained_bytes, working_limit)
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    if !evidence_policy.retention.docstrings {
        for file in &mut extracted.files {
            for symbol in &mut file.symbols {
                symbol.docstring = None;
            }
        }
    }
    analyze_partial_clones(
        CloneAnalysisInput {
            extracted: &mut extracted,
            source_root: &source_root,
            policy: clone_policy,
            budget: &mut budget,
        },
        &mut cancelled,
    )
    .map_err(|_| classify_resolve_failure(&budget))?;
    let index = build_resolution_index(
        &extracted,
        ResolutionIndexContext {
            source_root: &source_root,
            budget: &mut budget,
            cancelled: &mut cancelled,
        },
    )
    .map_err(|_| classify_resolve_failure(&budget))?;
    let diagnostics = extracted.diagnostics;
    let mut report = ResolutionReport {
        diagnostics,
        ..ResolutionReport::default()
    };
    let mut facts = GenerationFacts::default();
    reserve_generation_vectors(&mut facts, &extracted, &mut budget)
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    {
        let mut output = ResolutionOutput {
            index: &index,
            facts: &mut facts,
            report: &mut report,
            budget: &mut budget,
        };
        for file in extracted.files {
            output
                .append_file(file, &mut cancelled)
                .map_err(|_| classify_resolve_failure(output.budget))?;
        }
    }
    append_derived_resolution_evidence(DerivedResolutionEvidence {
        index: &index,
        facts: &mut facts,
        budget: &mut budget,
        policy: evidence_policy,
        cancelled: &mut cancelled,
    })
    .map_err(|_| classify_resolve_failure(&budget))?;
    // Unordered facts are the resolver's bounded working set. Canonical
    // validation below separately enforces `maximum_bytes` on the reduced
    // output, so applying that final-output ceiling here rejects inputs that
    // safely reduce well below it.
    let measurement = facts
        .measure_retained_bytes(working_limit, &mut cancelled)
        .map_err(classify_resolve_measurement_failure)?;
    budget
        .charge(measurement.transient_bytes())
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
    report.retained_bytes = measurement.retained_bytes();
    report.charged_high_water_bytes = budget.charged_bytes;
    Ok((facts, report))
}

fn classify_resolve_failure(budget: &ResolveBudget) -> ResolveGenerationFailure {
    if budget.charged_bytes > budget.maximum_bytes {
        ResolveGenerationFailure::generation_capacity_exceeded()
    } else {
        ResolveGenerationFailure::unclassified()
    }
}

const fn classify_resolve_measurement_failure(
    error: GenerationMemoryModelError,
) -> ResolveGenerationFailure {
    match error {
        GenerationMemoryModelError::RetainedLimit => {
            ResolveGenerationFailure::generation_capacity_exceeded()
        }
        GenerationMemoryModelError::Cancelled | GenerationMemoryModelError::MetadataDepth => {
            ResolveGenerationFailure::unclassified()
        }
    }
}

fn append_derived_resolution_evidence<Cancel>(
    input: DerivedResolutionEvidence<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let DerivedResolutionEvidence {
        index,
        facts,
        budget,
        policy,
        cancelled,
    } = input;
    append_framework_bridge_edges(ResolutionMutation {
        index,
        facts: &mut *facts,
        budget: &mut *budget,
        cancelled: &mut *cancelled,
    })?;
    append_go_structural_edges(GoStructuralEdges {
        index,
        facts: &mut *facts,
        budget: &mut *budget,
        cancelled: &mut *cancelled,
    })?;
    append_module_reexport_edges(ResolutionMutation {
        index,
        facts: &mut *facts,
        budget: &mut *budget,
        cancelled: &mut *cancelled,
    })?;
    append_test_subject_edges(ResolutionMutation {
        index,
        facts: &mut *facts,
        budget: &mut *budget,
        cancelled: &mut *cancelled,
    })?;
    if policy.centrality.page_rank {
        apply_page_rank(facts, &mut *cancelled).map_err(|_| StageItemFailure)?;
    }
    if policy.centrality.betweenness {
        apply_sampled_betweenness(facts, &mut *cancelled).map_err(|_| StageItemFailure)?;
    }
    if !policy.retention.call_sites {
        facts.references.clear();
    }
    Ok(())
}

struct CloneAnalysisCandidate {
    file_index: usize,
    symbol_index: usize,
    language: SourceLanguage,
    profile: Option<CloneTokenProfile>,
    prefilter: CloneProfilePrefilter,
    syntactic_claimed: bool,
    first_partial_band_hit: bool,
    partial_peer_count: u32,
    maximum_overlap_ppm: u32,
    minimum_overlap_ppm: u32,
    listed_peer_indexes: [Option<usize>; MAXIMUM_LISTED_CLONE_PEERS],
    listed_peer_count: usize,
    partial_component_parent: usize,
    partial_component_rank: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CloneProfilePrefilter {
    complete: [u32; CLONE_PREFILTER_BUCKETS],
    identifiers: [u32; CLONE_PREFILTER_BUCKETS],
}

impl CloneProfilePrefilter {
    fn from_profile(profile: &CloneTokenProfile) -> Self {
        Self {
            complete: clone_count_prefilter(profile.counts()),
            identifiers: clone_count_prefilter(profile.identifier_counts()),
        }
    }
}

#[derive(Clone, Copy)]
struct ClonePeerInput {
    peer: usize,
    overlap_ppm: u32,
    minimum_overlap_ppm: u32,
}

impl CloneAnalysisCandidate {
    fn push_peer(&mut self, input: ClonePeerInput) {
        self.partial_peer_count = self.partial_peer_count.saturating_add(1);
        self.maximum_overlap_ppm = self.maximum_overlap_ppm.max(input.overlap_ppm);
        self.minimum_overlap_ppm = input.minimum_overlap_ppm;
        if self.listed_peer_count < MAXIMUM_LISTED_CLONE_PEERS {
            self.listed_peer_indexes[self.listed_peer_count] = Some(input.peer);
            self.listed_peer_count = self.listed_peer_count.saturating_add(1);
        }
    }
}

fn analyze_partial_clones<Cancel>(
    input: CloneAnalysisInput<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let CloneAnalysisInput {
        extracted,
        source_root,
        policy,
        budget,
    } = input;
    let mut candidates = collect_clone_candidates(
        CloneCandidateCollectionInput {
            extracted: &mut *extracted,
            source_root,
            budget: &mut *budget,
        },
        cancelled,
    )?;
    if candidates.len() < 2 {
        return Ok(());
    }
    let order_bytes = usize_to_u64(candidates.len())
        .saturating_mul(usize_to_u64(size_of::<usize>()))
        .saturating_mul(3);
    budget.charge(order_bytes)?;
    mark_exact_clone_candidates(extracted, &mut candidates, cancelled)?;
    mark_near_clone_candidates(extracted, &mut candidates, cancelled)?;
    compare_partial_clone_band(PartialCloneBand {
        extracted,
        candidates: &mut candidates,
        minimum_overlap_ppm: DUPLICATE_PARTIAL_DEFAULT_OVERLAP_PPM,
        exclude_first_band: false,
        cancelled,
    })?;
    if policy.wider_partial_band {
        compare_partial_clone_band(PartialCloneBand {
            extracted,
            candidates: &mut candidates,
            minimum_overlap_ppm: DUPLICATE_PARTIAL_WIDER_OVERLAP_PPM,
            exclude_first_band: true,
            cancelled,
        })?;
    }
    persist_partial_clone_evidence(PersistPartialClones {
        extracted,
        candidates: &candidates,
        budget,
        cancelled,
    })
}

struct CloneEvidencePatch {
    duplicate_detection_enabled: bool,
    near_clone_compatibility: NearCloneCompatibility,
    partial_clone: Option<PartialCloneEvidence>,
}

type CloneEvidenceMap = BTreeMap<SymbolId, CloneEvidencePatch>;

struct SpilledResolutionPreparation {
    compact: NativeFactAccumulator,
    index: ResolutionIndex,
    budget: ResolveBudget,
}

async fn build_spilled_resolution_preparation(
    source: &SpilledNativeFacts,
    source_root: &SourceRoot,
    request: ResolutionPreparationRequest<'_>,
) -> Result<(CloneEvidenceMap, ResolutionIndex, u64), StageItemFailure> {
    let ResolutionPreparationRequest {
        maximum_bytes,
        cancellation,
        progress,
        ..
    } = request;
    let working_limit = maximum_bytes
        .checked_mul(RESOLVE_WORKING_MULTIPLIER)
        .ok_or(StageItemFailure)?;
    let mut preparation = SpilledResolutionPreparation {
        compact: NativeFactAccumulator::new(maximum_bytes),
        index: ResolutionIndex::default(),
        budget: ResolveBudget::new(0, working_limit)?,
    };
    visit_spilled_native_files(
        SpilledFileWalk {
            source,
            observation: SpilledVisitObservation {
                cancellation,
                progress,
            },
        },
        &mut preparation,
        |state, file| {
            let mut cancelled = || cancellation.is_cancelled();
            index_resolution_file_metadata(ResolutionIndexFileInput {
                index: &mut state.index,
                file: &file,
                budget: &mut state.budget,
                cancelled: &mut cancelled,
            })?;
            let mut context = ResolutionIndexContext {
                source_root,
                budget: &mut state.budget,
                cancelled: &mut cancelled,
            };
            block_in_place(|| {
                qualtype_resolution::index_syntax(&mut state.index, &file, &mut context)
            })?;
            index_typescript_alias_file(&mut state.index.modules, &file, &mut context)?;
            let before = state.compact.retained_bytes;
            let diagnostics = state.compact.diagnostics;
            state
                .compact
                .push_compact_clone(compact_clone_file(file), diagnostics)?;
            state
                .budget
                .charge(state.compact.retained_bytes.saturating_sub(before))
        },
    )
    .await?;
    visit_spilled_native_files(
        SpilledFileWalk {
            source,
            observation: SpilledVisitObservation {
                cancellation,
                progress,
            },
        },
        &mut preparation,
        |state, file| {
            let mut cancelled = || cancellation.is_cancelled();
            index_rust_workspace_package_file(
                &mut ResolutionIndexTarget {
                    index: &mut state.index,
                    budget: &mut state.budget,
                },
                &file,
                &mut cancelled,
            )?;
            index_resolution_file_symbols(ResolutionIndexFileInput {
                index: &mut state.index,
                file: &file,
                budget: &mut state.budget,
                cancelled: &mut cancelled,
            })
        },
    )
    .await?;
    // Candidate ordering and partial-clone analysis walk every retained symbol
    // and reread sources; keep that synchronous work off the async worker core.
    block_in_place(move || finish_spilled_resolution_preparation(preparation, source_root, request))
}

/// Order resolution candidates, analyze partial clones, and collect clone
/// evidence for a fully indexed spilled generation.
fn finish_spilled_resolution_preparation(
    mut preparation: SpilledResolutionPreparation,
    source_root: &SourceRoot,
    request: ResolutionPreparationRequest<'_>,
) -> Result<(CloneEvidenceMap, ResolutionIndex, u64), StageItemFailure> {
    let ResolutionPreparationRequest {
        policy,
        cancellation,
        ..
    } = request;
    let mut cancelled = || cancellation.is_cancelled();
    javascript_config::finish(
        &mut preparation.index.modules,
        &mut ResolutionIndexContext {
            source_root,
            budget: &mut preparation.budget,
            cancelled: &mut cancelled,
        },
    )?;
    finalize_resolution_candidate_order(
        &mut preparation.index,
        &mut preparation.budget,
        &mut cancelled,
    )?;
    javascript_exports::prepare(
        &mut preparation.index,
        &mut preparation.budget,
        &mut cancelled,
    )?;
    analyze_partial_clones(
        CloneAnalysisInput {
            extracted: &mut preparation.compact,
            source_root,
            policy,
            budget: &mut preparation.budget,
        },
        &mut cancelled,
    )?;
    let evidence =
        finish_spilled_clone_evidence(preparation.compact, &mut preparation.budget, cancellation)?;
    Ok((
        evidence,
        preparation.index,
        preparation.budget.charged_bytes,
    ))
}

fn finish_spilled_clone_evidence(
    compact: NativeFactAccumulator,
    budget: &mut ResolveBudget,
    cancellation: &StageCancellation,
) -> Result<CloneEvidenceMap, StageItemFailure> {
    let mut evidence = CloneEvidenceMap::new();
    for file in compact.files {
        for mut symbol in file.symbols {
            if cancellation.is_cancelled() {
                return Err(StageItemFailure);
            }
            let patch_bytes = RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<CloneEvidencePatch>()))
                .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len()));
            budget.charge(patch_bytes)?;
            let symbol_id = symbol.input.symbol_id.clone();
            if evidence
                .insert(
                    symbol_id,
                    CloneEvidencePatch {
                        duplicate_detection_enabled: symbol.duplicate_detection_enabled,
                        near_clone_compatibility: symbol.near_clone_compatibility,
                        partial_clone: symbol.partial_clone.take(),
                    },
                )
                .is_some()
            {
                return Err(StageItemFailure);
            }
        }
    }
    Ok(evidence)
}

fn apply_clone_evidence(
    file: &mut NativeFileFacts,
    evidence: &mut CloneEvidenceMap,
) -> Result<(), StageItemFailure> {
    for symbol in &mut file.symbols {
        let patch = evidence
            .remove(&symbol.input.symbol_id)
            .ok_or(StageItemFailure)?;
        symbol.duplicate_detection_enabled = patch.duplicate_detection_enabled;
        symbol.near_clone_compatibility = patch.near_clone_compatibility;
        symbol.partial_clone = patch.partial_clone;
        symbol.clone_token_profile = None;
    }
    Ok(())
}

fn collect_clone_candidates<Cancel>(
    input: CloneCandidateCollectionInput<'_>,
    cancelled: &mut Cancel,
) -> Result<Vec<CloneAnalysisCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let CloneCandidateCollectionInput {
        extracted,
        source_root,
        budget,
    } = input;
    let symbol_count = extracted
        .files
        .iter()
        .map(|file| file.symbols.len())
        .sum::<usize>();
    let mut candidates = Vec::new();
    candidates
        .try_reserve_exact(symbol_count)
        .map_err(|_| StageItemFailure)?;
    budget.charge(
        usize_to_u64(symbol_count)
            .saturating_mul(usize_to_u64(size_of::<CloneAnalysisCandidate>())),
    )?;
    for (file_index, file) in extracted.files.iter_mut().enumerate() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let production = is_production_clone_path(&file.file.normalized_path);
        let enabled =
            production && !source_root.duplicate_code_allowlisted(&file.file.normalized_path);
        let language =
            SourceLanguage::from_stable_str(&file.file.language).ok_or(StageItemFailure)?;
        for (symbol_index, symbol) in file.symbols.iter_mut().enumerate() {
            symbol.duplicate_detection_enabled = enabled;
            if !enabled
                || !matches!(
                    symbol.kind,
                    SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
                )
                || symbol
                    .input
                    .end_line
                    .saturating_sub(symbol.input.start_line)
                    .saturating_add(1)
                    < DUPLICATE_MINIMUM_LINES
            {
                symbol.clone_token_profile = None;
                continue;
            }
            let profile = symbol.clone_token_profile.take();
            let prefilter = profile.as_ref().map_or_else(
                CloneProfilePrefilter::default,
                CloneProfilePrefilter::from_profile,
            );
            let candidate_index = candidates.len();
            candidates.push(CloneAnalysisCandidate {
                file_index,
                symbol_index,
                language,
                profile,
                prefilter,
                syntactic_claimed: false,
                first_partial_band_hit: false,
                partial_peer_count: 0,
                maximum_overlap_ppm: 0,
                minimum_overlap_ppm: 0,
                listed_peer_indexes: [None; MAXIMUM_LISTED_CLONE_PEERS],
                listed_peer_count: 0,
                partial_component_parent: candidate_index,
                partial_component_rank: 0,
            });
        }
    }
    Ok(candidates)
}

fn mark_exact_clone_candidates<Cancel>(
    extracted: &NativeFactAccumulator,
    candidates: &mut [CloneAnalysisCandidate],
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut order = clone_candidate_order(candidates.len())?;
    order.sort_unstable_by(|left, right| {
        clone_symbol(extracted, &candidates[*left])
            .input
            .structural_digest
            .cmp(
                &clone_symbol(extracted, &candidates[*right])
                    .input
                    .structural_digest,
            )
            .then_with(|| {
                clone_symbol_id(extracted, &candidates[*left])
                    .cmp(clone_symbol_id(extracted, &candidates[*right]))
            })
    });
    let mut start = 0_usize;
    while start < order.len() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let digest = &clone_symbol(extracted, &candidates[order[start]])
            .input
            .structural_digest;
        let mut end = start.saturating_add(1);
        while end < order.len()
            && clone_symbol(extracted, &candidates[order[end]])
                .input
                .structural_digest
                == *digest
        {
            end = end.saturating_add(1);
        }
        if end.saturating_sub(start) > 1 {
            for candidate in &order[start..end] {
                candidates[*candidate].syntactic_claimed = true;
            }
        }
        start = end;
    }
    Ok(())
}

fn mark_near_clone_candidates<Cancel>(
    extracted: &mut NativeFactAccumulator,
    candidates: &mut [CloneAnalysisCandidate],
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut order = clone_candidate_order(candidates.len())?;
    order.retain(|index| {
        !candidates[*index].syntactic_claimed
            && clone_symbol_lines(extracted, &candidates[*index]) >= DUPLICATE_NEAR_MINIMUM_LINES
            && !clone_symbol_literal_heavy(extracted, &candidates[*index])
    });
    order.sort_unstable_by(|left, right| {
        clone_symbol(extracted, &candidates[*left])
            .clone_shape_digest
            .cmp(&clone_symbol(extracted, &candidates[*right]).clone_shape_digest)
            .then_with(|| {
                clone_symbol_id(extracted, &candidates[*left])
                    .cmp(clone_symbol_id(extracted, &candidates[*right]))
            })
    });
    let mut start = 0_usize;
    while start < order.len() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let shape = &clone_symbol(extracted, &candidates[order[start]]).clone_shape_digest;
        let mut end = start.saturating_add(1);
        while end < order.len()
            && clone_symbol(extracted, &candidates[order[end]]).clone_shape_digest == *shape
        {
            end = end.saturating_add(1);
        }
        mark_near_clone_group(extracted, candidates, &order[start..end]);
        start = end;
    }
    Ok(())
}

fn mark_near_clone_group(
    extracted: &mut NativeFactAccumulator,
    candidates: &mut [CloneAnalysisCandidate],
    group: &[usize],
) {
    if group.len() <= 1 || !near_clone_group_compatible(extracted, candidates, group) {
        return;
    }
    for candidate in group {
        candidates[*candidate].syntactic_claimed = true;
        extracted.files[candidates[*candidate].file_index].symbols
            [candidates[*candidate].symbol_index]
            .near_clone_compatibility = NearCloneCompatibility::Compatible;
    }
}

fn near_clone_group_compatible(
    extracted: &NativeFactAccumulator,
    candidates: &[CloneAnalysisCandidate],
    group: &[usize],
) -> bool {
    let first_digest = &clone_symbol(extracted, &candidates[group[0]])
        .input
        .structural_digest;
    let spans_multiple_exact_digests = group[1..].iter().any(|index| {
        clone_symbol(extracted, &candidates[*index])
            .input
            .structural_digest
            != *first_digest
    });
    spans_multiple_exact_digests
        && group.iter().enumerate().all(|(left_position, left)| {
            group
                .iter()
                .skip(left_position.saturating_add(1))
                .all(|right| {
                    clone_candidates_semantically_compatible(
                        &candidates[*left],
                        &candidates[*right],
                    )
                })
        })
}

fn compare_partial_clone_band<Cancel>(
    input: PartialCloneBand<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let PartialCloneBand {
        extracted,
        candidates,
        minimum_overlap_ppm,
        exclude_first_band,
        cancelled,
    } = input;
    let mut order = clone_candidate_order(candidates.len())?;
    order.retain(|index| {
        let candidate = &candidates[*index];
        !candidate.syntactic_claimed
            && (!exclude_first_band || !candidate.first_partial_band_hit)
            && candidate.profile.is_some()
            && clone_symbol_lines(extracted, candidate) >= DUPLICATE_NEAR_MINIMUM_LINES
            && !clone_symbol_literal_heavy(extracted, candidate)
    });
    order.sort_unstable_by(|left, right| {
        let left_candidate = &candidates[*left];
        let right_candidate = &candidates[*right];
        left_candidate
            .language
            .cmp(&right_candidate.language)
            .then_with(|| {
                clone_profile_total(left_candidate).cmp(&clone_profile_total(right_candidate))
            })
            .then_with(|| {
                clone_symbol_id(extracted, left_candidate)
                    .cmp(clone_symbol_id(extracted, right_candidate))
            })
    });
    for left_position in 0..order.len() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        compare_partial_clone_candidate(PartialCloneCandidateInput {
            candidates: &mut *candidates,
            order: &order,
            left_position,
            minimum_overlap_ppm,
        })?;
    }
    Ok(())
}

struct PartialCloneCandidateInput<'a> {
    candidates: &'a mut [CloneAnalysisCandidate],
    order: &'a [usize],
    left_position: usize,
    minimum_overlap_ppm: u32,
}

fn compare_partial_clone_candidate(
    input: PartialCloneCandidateInput<'_>,
) -> Result<(), StageItemFailure> {
    let PartialCloneCandidateInput {
        candidates,
        order,
        left_position,
        minimum_overlap_ppm,
    } = input;
    let left_index = order[left_position];
    let left_language = candidates[left_index].language;
    let left_total = clone_profile_total(&candidates[left_index]);
    for right_index in order.iter().copied().skip(left_position.saturating_add(1)) {
        if candidates[right_index].language != left_language {
            break;
        }
        let right_total = clone_profile_total(&candidates[right_index]);
        if partial_clone_band_exhausted(left_total, right_total, minimum_overlap_ppm) {
            break;
        }
        if clone_prefilter_overlap_ppm(
            ClonePrefilter {
                buckets: &candidates[left_index].prefilter.complete,
                total: left_total,
            },
            ClonePrefilter {
                buckets: &candidates[right_index].prefilter.complete,
                total: right_total,
            },
        ) < minimum_overlap_ppm
        {
            continue;
        }
        let overlap_ppm = clone_profile_overlap_ppm(
            candidates[left_index]
                .profile
                .as_ref()
                .ok_or(StageItemFailure)?,
            candidates[right_index]
                .profile
                .as_ref()
                .ok_or(StageItemFailure)?,
        );
        if overlap_ppm < minimum_overlap_ppm
            || !clone_candidates_semantically_compatible(
                &candidates[left_index],
                &candidates[right_index],
            )
        {
            continue;
        }
        link_partial_candidates(PartialCloneLink {
            candidates,
            left: left_index,
            right: right_index,
            overlap_ppm,
            minimum_overlap_ppm,
        })?;
    }
    Ok(())
}

fn partial_clone_band_exhausted(left_total: u32, right_total: u32, overlap_ppm: u32) -> bool {
    u64::from(right_total).saturating_mul(u64::from(overlap_ppm))
        > u64::from(left_total).saturating_mul(1_000_000)
}

fn link_partial_candidates(input: PartialCloneLink<'_>) -> Result<(), StageItemFailure> {
    let PartialCloneLink {
        candidates,
        left,
        right,
        overlap_ppm,
        minimum_overlap_ppm,
    } = input;
    union_partial_clone_components(candidates, left, right)?;
    let (left_candidate, right_candidate) = if left < right {
        let (before_right, from_right) = candidates.split_at_mut(right);
        (&mut before_right[left], &mut from_right[0])
    } else {
        let (before_left, from_left) = candidates.split_at_mut(left);
        (&mut from_left[0], &mut before_left[right])
    };
    left_candidate.push_peer(ClonePeerInput {
        peer: right,
        overlap_ppm,
        minimum_overlap_ppm,
    });
    right_candidate.push_peer(ClonePeerInput {
        peer: left,
        overlap_ppm,
        minimum_overlap_ppm,
    });
    if minimum_overlap_ppm == DUPLICATE_PARTIAL_DEFAULT_OVERLAP_PPM {
        left_candidate.first_partial_band_hit = true;
        right_candidate.first_partial_band_hit = true;
    }
    Ok(())
}

fn partial_clone_component_root(
    candidates: &[CloneAnalysisCandidate],
    mut index: usize,
) -> Result<usize, StageItemFailure> {
    for _ in 0..=candidates.len() {
        let candidate = candidates.get(index).ok_or(StageItemFailure)?;
        if candidate.partial_component_parent == index {
            return Ok(index);
        }
        index = candidate.partial_component_parent;
    }
    Err(StageItemFailure)
}

fn union_partial_clone_components(
    candidates: &mut [CloneAnalysisCandidate],
    left: usize,
    right: usize,
) -> Result<(), StageItemFailure> {
    let mut left_root = partial_clone_component_root(candidates, left)?;
    let mut right_root = partial_clone_component_root(candidates, right)?;
    if left_root == right_root {
        return Ok(());
    }
    let left_rank = candidates
        .get(left_root)
        .ok_or(StageItemFailure)?
        .partial_component_rank;
    let right_rank = candidates
        .get(right_root)
        .ok_or(StageItemFailure)?
        .partial_component_rank;
    if left_rank < right_rank {
        std::mem::swap(&mut left_root, &mut right_root);
    }
    candidates
        .get_mut(right_root)
        .ok_or(StageItemFailure)?
        .partial_component_parent = left_root;
    if left_rank == right_rank {
        let root = candidates.get_mut(left_root).ok_or(StageItemFailure)?;
        root.partial_component_rank = root.partial_component_rank.saturating_add(1);
    }
    Ok(())
}

fn persist_partial_clone_evidence<Cancel>(
    input: PersistPartialClones<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let PersistPartialClones {
        extracted,
        candidates,
        budget,
        cancelled,
    } = input;
    if candidates
        .iter()
        .all(|candidate| candidate.partial_peer_count == 0)
    {
        return Ok(());
    }
    let component_slot_bytes = usize_to_u64(candidates.len()).saturating_mul(
        usize_to_u64(size_of::<Option<usize>>()).saturating_add(usize_to_u64(size_of::<u32>())),
    );
    budget.charge(component_slot_bytes)?;
    let mut component_representatives = Vec::new();
    component_representatives
        .try_reserve_exact(candidates.len())
        .map_err(|_| StageItemFailure)?;
    component_representatives.resize(candidates.len(), None);
    let mut component_sizes = Vec::new();
    component_sizes
        .try_reserve_exact(candidates.len())
        .map_err(|_| StageItemFailure)?;
    component_sizes.resize(candidates.len(), 0_u32);
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        if candidate.partial_peer_count == 0 {
            continue;
        }
        let root = partial_clone_component_root(candidates, candidate_index)?;
        component_sizes[root] = component_sizes[root].saturating_add(1);
        let replace = component_representatives[root].is_none_or(|representative| {
            clone_symbol_id(extracted, candidate)
                < clone_symbol_id(extracted, &candidates[representative])
        });
        if replace {
            component_representatives[root] = Some(candidate_index);
        }
    }
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.partial_peer_count == 0 {
            continue;
        }
        let mut peer_indexes = candidate.listed_peer_indexes[..candidate.listed_peer_count]
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        peer_indexes.sort_unstable_by(|left, right| {
            clone_symbol_id(extracted, &candidates[*left])
                .cmp(clone_symbol_id(extracted, &candidates[*right]))
        });
        let mut listed_peers = Vec::new();
        listed_peers
            .try_reserve_exact(peer_indexes.len())
            .map_err(|_| StageItemFailure)?;
        budget.charge(
            usize_to_u64(peer_indexes.len()).saturating_mul(
                usize_to_u64(size_of::<SymbolId>()).saturating_add(UUID_TEXT_BYTES),
            ),
        )?;
        for peer in peer_indexes {
            listed_peers.push(clone_symbol_id(extracted, &candidates[peer]).clone());
        }
        let component_root = partial_clone_component_root(candidates, candidate_index)?;
        let representative_index =
            component_representatives[component_root].ok_or(StageItemFailure)?;
        budget.charge(usize_to_u64(size_of::<SymbolId>()).saturating_add(UUID_TEXT_BYTES))?;
        let representative = clone_symbol_id(extracted, &candidates[representative_index]).clone();
        let component_size = component_sizes[component_root];
        extracted.files[candidate.file_index].symbols[candidate.symbol_index].partial_clone =
            Some(PartialCloneEvidence {
                peer_count: candidate.partial_peer_count,
                component_size,
                maximum_overlap_ppm: candidate.maximum_overlap_ppm,
                minimum_overlap_ppm: candidate.minimum_overlap_ppm,
                representative,
                listed_peers,
            });
    }
    Ok(())
}

fn clone_candidate_order(length: usize) -> Result<Vec<usize>, StageItemFailure> {
    let mut order = Vec::new();
    order
        .try_reserve_exact(length)
        .map_err(|_| StageItemFailure)?;
    order.extend(0..length);
    Ok(order)
}

fn clone_symbol<'facts>(
    extracted: &'facts NativeFactAccumulator,
    candidate: &CloneAnalysisCandidate,
) -> &'facts NativeSymbolFacts {
    &extracted.files[candidate.file_index].symbols[candidate.symbol_index]
}

fn clone_symbol_id<'facts>(
    extracted: &'facts NativeFactAccumulator,
    candidate: &CloneAnalysisCandidate,
) -> &'facts SymbolId {
    &clone_symbol(extracted, candidate).input.symbol_id
}

fn clone_symbol_lines(
    extracted: &NativeFactAccumulator,
    candidate: &CloneAnalysisCandidate,
) -> u32 {
    let symbol = clone_symbol(extracted, candidate);
    symbol
        .input
        .end_line
        .saturating_sub(symbol.input.start_line)
        .saturating_add(1)
}

fn clone_symbol_literal_heavy(
    extracted: &NativeFactAccumulator,
    candidate: &CloneAnalysisCandidate,
) -> bool {
    let symbol = clone_symbol(extracted, candidate);
    let bytes = symbol
        .input
        .end_byte
        .saturating_sub(symbol.input.start_byte)
        .max(1);
    u64::from(symbol.health.literal_bytes).saturating_mul(1_000_000)
        > bytes.saturating_mul(DUPLICATE_MAXIMUM_LITERAL_RATIO_PPM)
}

fn clone_profile_total(candidate: &CloneAnalysisCandidate) -> u32 {
    candidate
        .profile
        .as_ref()
        .map_or(0, CloneTokenProfile::total_tokens)
}

#[derive(Clone, Copy)]
struct CloneProfileView<'profile> {
    counts: &'profile [CloneTokenCount],
    total: u32,
}

fn clone_multiset_overlap_ppm(left: CloneProfileView<'_>, right: CloneProfileView<'_>) -> u32 {
    let mut left_index = 0_usize;
    let mut right_index = 0_usize;
    let mut intersection = 0_u64;
    while left_index < left.counts.len() && right_index < right.counts.len() {
        let left_count = left.counts[left_index];
        let right_count = right.counts[right_index];
        match left_count.0.cmp(&right_count.0) {
            std::cmp::Ordering::Less => left_index = left_index.saturating_add(1),
            std::cmp::Ordering::Greater => right_index = right_index.saturating_add(1),
            std::cmp::Ordering::Equal => {
                intersection =
                    intersection.saturating_add(u64::from(left_count.1.min(right_count.1)));
                left_index = left_index.saturating_add(1);
                right_index = right_index.saturating_add(1);
            }
        }
    }
    let maximum = u64::from(left.total.max(right.total)).max(1);
    u32::try_from(intersection.saturating_mul(1_000_000) / maximum).unwrap_or(u32::MAX)
}

fn clone_count_prefilter(counts: &[CloneTokenCount]) -> [u32; CLONE_PREFILTER_BUCKETS] {
    let mut buckets = [0_u32; CLONE_PREFILTER_BUCKETS];
    for count in counts {
        let bucket = usize::try_from(count.0 >> CLONE_PREFILTER_SHIFT).unwrap_or(0);
        buckets[bucket] = buckets[bucket].saturating_add(count.1);
    }
    buckets
}

fn clone_prefilter_overlap_ppm(left: ClonePrefilter<'_>, right: ClonePrefilter<'_>) -> u32 {
    let (left_total, right_total) = (left.total, right.total);
    let (left, right) = (left.buckets, right.buckets);
    let intersection = left
        .iter()
        .zip(right)
        .map(|(left, right)| u64::from((*left).min(*right)))
        .sum::<u64>();
    let maximum = u64::from(left_total.max(right_total)).max(1);
    u32::try_from(intersection.saturating_mul(1_000_000) / maximum).unwrap_or(u32::MAX)
}

fn complete_clone_profile(profile: &CloneTokenProfile) -> CloneProfileView<'_> {
    CloneProfileView {
        counts: profile.counts(),
        total: profile.total_tokens(),
    }
}

fn identifier_clone_profile(profile: &CloneTokenProfile) -> CloneProfileView<'_> {
    CloneProfileView {
        counts: profile.identifier_counts(),
        total: profile.identifier_tokens(),
    }
}

fn clone_profile_overlap_ppm(left: &CloneTokenProfile, right: &CloneTokenProfile) -> u32 {
    clone_multiset_overlap_ppm(complete_clone_profile(left), complete_clone_profile(right))
}

fn clone_identifier_overlap_ppm(left: &CloneTokenProfile, right: &CloneTokenProfile) -> u32 {
    clone_multiset_overlap_ppm(
        identifier_clone_profile(left),
        identifier_clone_profile(right),
    )
}

fn clone_candidates_semantically_compatible(
    left: &CloneAnalysisCandidate,
    right: &CloneAnalysisCandidate,
) -> bool {
    match (left.profile.as_ref(), right.profile.as_ref()) {
        (Some(left_profile), Some(right_profile)) => {
            let minimum_overlap = if left.file_index == right.file_index {
                DUPLICATE_SAME_FILE_IDENTIFIER_OVERLAP_PPM
            } else {
                DUPLICATE_IDENTIFIER_MINIMUM_OVERLAP_PPM
            };
            if clone_prefilter_overlap_ppm(
                ClonePrefilter {
                    buckets: &left.prefilter.identifiers,
                    total: left_profile.identifier_tokens(),
                },
                ClonePrefilter {
                    buckets: &right.prefilter.identifiers,
                    total: right_profile.identifier_tokens(),
                },
            ) < minimum_overlap
            {
                return false;
            }
            let overlap = clone_identifier_overlap_ppm(left_profile, right_profile);
            overlap >= DUPLICATE_IDENTIFIER_MINIMUM_OVERLAP_PPM
                || (left.file_index == right.file_index
                    && overlap >= DUPLICATE_SAME_FILE_IDENTIFIER_OVERLAP_PPM)
        }
        _ => false,
    }
}

fn is_production_clone_path(path: &str) -> bool {
    if is_test_source_path(path) {
        return false;
    }
    let lowercase = path.to_ascii_lowercase();
    let components = lowercase.split('/').collect::<Vec<_>>();
    if components.iter().any(|component| {
        matches!(
            *component,
            "fixture"
                | "fixtures"
                | "test-bed"
                | "test-beds"
                | "__mocks__"
                | "integration"
                | "testing"
                | "testlib"
                | "test"
                | "tests"
                | "spec"
                | "specs"
                | "__tests__"
        ) || component.starts_with("bench")
            || matches!(
                *component,
                "script"
                    | "scripts"
                    | "example"
                    | "examples"
                    | "sample"
                    | "samples"
                    | "demo"
                    | "demos"
            )
    }) {
        return false;
    }
    let filename = components.last().copied().unwrap_or_default();
    if [
        ".test.",
        ".spec.",
        "_test.",
        "_spec.",
        ".gen.",
        ".generated.",
    ]
    .into_iter()
    .any(|marker| filename.contains(marker))
        || filename.starts_with("test-")
        || filename.starts_with("test_")
        || matches!(
            filename.split('.').next().unwrap_or_default(),
            "test" | "mock" | "mocks" | "fixture" | "fixtures"
        )
    {
        return false;
    }
    if !path.contains('/') {
        let stem = filename.split('.').next().unwrap_or_default();
        if matches!(
            stem,
            "publish" | "release" | "build" | "deploy" | "bundle" | "prepublish" | "postinstall"
        ) {
            return false;
        }
    }
    let original_stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .split('.')
        .next()
        .unwrap_or_default();
    !["Test", "Tests", "TestCase", "Spec"]
        .into_iter()
        .any(|suffix| original_stem.ends_with(suffix) && original_stem.len() > suffix.len())
}

struct FrameworkCandidateMutation<'context, Cancel> {
    index: &'context ResolutionIndex,
    facts: &'context mut GenerationFacts,
    budget: &'context mut ResolveBudget,
    candidates: &'context [ResolutionCandidate],
    cancelled: &'context mut Cancel,
}

#[derive(Clone, Copy)]
struct FrameworkEdgeInput<'context> {
    source: &'context ResolutionCandidate,
    target: &'context ResolutionCandidate,
    confidence: f32,
    provenance: &'static str,
}

struct ManifestWorkspaceExclusion<'context, Cancel> {
    index: &'context ResolutionIndex,
    workspace: ManifestWorkspaceTag<'context>,
    package_directory: &'context str,
    cancelled: &'context mut Cancel,
}

struct DrupalServiceQuery<'context, Cancel> {
    index: &'context ResolutionIndex,
    file_id: &'context FileId,
    service_id: &'context str,
    cancelled: &'context mut Cancel,
}

struct TurboTargetQuery<'context, 'candidate, Cancel> {
    index: &'context ResolutionIndex,
    candidates: &'candidate [ResolutionCandidate],
    module: &'context str,
    cancelled: &'context mut Cancel,
}

struct TurboTargetTierQuery<'context, 'candidate, Cancel> {
    index: &'context ResolutionIndex,
    candidates: &'candidate [ResolutionCandidate],
    module: &'context str,
    languages: &'context [&'context str],
    tagged: bool,
    cancelled: &'context mut Cancel,
}

#[derive(Clone, Copy)]
struct TaggedCandidateQuery<'context, 'candidate> {
    index: &'context ResolutionIndex,
    candidates: &'candidate [ResolutionCandidate],
}

fn append_framework_bridge_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    append_manifest_workspace_edges(ResolutionMutation {
        index,
        facts: &mut *facts,
        budget: &mut *budget,
        cancelled: &mut *cancelled,
    })?;
    append_mybatis_bridge_edges(ResolutionMutation {
        index,
        facts: &mut *facts,
        budget: &mut *budget,
        cancelled: &mut *cancelled,
    })?;
    append_named_framework_bridges(ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    })
}

fn append_mybatis_bridge_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    for (qualified_name, candidates) in ordered_resolution_candidates(index) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !qualified_name.contains("::") {
            continue;
        }
        append_mybatis_qualified_edges(
            ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            qualified_name,
            candidates,
        )?;
        append_mybatis_packaged_edges(
            ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            qualified_name,
            candidates,
        )?;
    }
    Ok(())
}

/// Bind a packaged JVM mapper method (`com.example::OrderMapper::findOrder`) to
/// the XML statement of the mapper whose namespace is exactly
/// `com.example.OrderMapper`. The XML statement keeps the simple
/// `OrderMapper::findOrder` name, so its exact JVM name is derived from the
/// mapper namespace symbol that contains it; nothing is matched by suffix.
fn append_mybatis_packaged_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
    qualified_name: &str,
    candidates: &[ResolutionCandidate],
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    for target in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if target.qualified_name != qualified_name {
            continue;
        }
        let Some(jvm_name) = mybatis_jvm_statement_name(index, target, cancelled)? else {
            continue;
        };
        let Some(sources) = index.candidates.get(&jvm_name) else {
            continue;
        };
        for source in sources.as_slice() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if !mybatis_jvm_mapper_method(index, (source, &jvm_name))? {
                continue;
            }
            append_framework_edge(
                facts,
                budget,
                FrameworkEdgeInput {
                    source,
                    target,
                    confidence: EXACT_PROJECT_CONFIDENCE,
                    provenance: MYBATIS_BRIDGE_PROVENANCE,
                },
            )?;
        }
    }
    Ok(())
}

fn mybatis_jvm_mapper_method(
    index: &ResolutionIndex,
    source: (&ResolutionCandidate, &str),
) -> Result<bool, StageItemFailure> {
    let (candidate, name) = source;
    let file = index
        .modules
        .files
        .get(&candidate.file_id)
        .ok_or(StageItemFailure)?;
    Ok(candidate.qualified_name == name
        && candidate.kind == SymbolKind::Method
        && matches!(file.language.as_str(), "java" | "kotlin" | "scala"))
}

/// The exact JVM qualified name of a `MyBatis` XML statement whose mapper
/// namespace is package-qualified, or `None` for any other candidate.
fn mybatis_jvm_statement_name<Cancel>(
    index: &ResolutionIndex,
    statement: &ResolutionCandidate,
    cancelled: &mut Cancel,
) -> Result<Option<String>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(file) = index.modules.files.get(&statement.file_id) else {
        return Err(StageItemFailure);
    };
    if file.language != SourceLanguage::Xml.as_str() || statement.kind != SymbolKind::Method {
        return Ok(None);
    }
    let (Some(parent), Some((mapper, _))) = (
        statement.parent_symbol_id.as_ref(),
        statement.qualified_name.split_once("::"),
    ) else {
        return Ok(None);
    };
    let mut namespace = None;
    for candidate in resolution_candidates_for_file(index, mapper, &statement.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if &candidate.symbol_id == parent && candidate.kind == SymbolKind::Namespace {
            namespace = Some(candidate);
            break;
        }
    }
    let Some((package, simple)) =
        namespace.and_then(|namespace| namespace.qualified_name.rsplit_once('.'))
    else {
        return Ok(None);
    };
    if package.is_empty() || simple != mapper {
        return Ok(None);
    }
    Ok(Some(format!("{package}::{}", statement.qualified_name)))
}

fn append_mybatis_qualified_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
    qualified_name: &str,
    candidates: &[ResolutionCandidate],
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    let targets = mybatis_xml_targets((index, qualified_name, candidates), budget, cancelled)?;
    if targets.is_empty() {
        return Ok(());
    }
    for source in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !mybatis_jvm_mapper_method(index, (source, qualified_name))? {
            continue;
        }
        append_mybatis_mapper_edges(
            ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            source,
            &targets,
        )?;
    }
    Ok(())
}

fn append_mybatis_mapper_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
    source: &ResolutionCandidate,
    targets: &[&ResolutionCandidate],
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    for &target in targets {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if source.symbol_id == target.symbol_id {
            continue;
        }
        // An unpackaged JVM mapper bound to a package-qualified namespace
        // matches only by the simple-namespace convention, not exactly.
        let confidence = if mybatis_jvm_statement_name(index, target, cancelled)?.is_some() {
            FRAMEWORK_CONVENTION_CONFIDENCE
        } else {
            EXACT_PROJECT_CONFIDENCE
        };
        append_framework_edge(
            facts,
            budget,
            FrameworkEdgeInput {
                source,
                target,
                confidence,
                provenance: MYBATIS_BRIDGE_PROVENANCE,
            },
        )?;
    }
    Ok(())
}

/// Select exact XML statements once, before scanning JVM mapper sources.
fn mybatis_xml_targets<'candidate, Cancel>(
    input: (&ResolutionIndex, &str, &'candidate [ResolutionCandidate]),
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<Vec<&'candidate ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (index, name, candidates) = input;
    let mut targets = Vec::new();
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.qualified_name != name || candidate.kind != SymbolKind::Method {
            continue;
        }
        let file = index
            .modules
            .files
            .get(&candidate.file_id)
            .ok_or(StageItemFailure)?;
        if file.language != SourceLanguage::Xml.as_str() {
            continue;
        }
        reserve_mybatis_target_slot(&mut targets, budget)?;
        targets.push(candidate);
    }
    Ok(targets)
}

fn reserve_mybatis_target_slot(
    targets: &mut Vec<&ResolutionCandidate>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if targets.len() < targets.capacity() {
        return Ok(());
    }
    let additional = targets.capacity().max(1);
    budget.charge(
        usize_to_u64(additional).saturating_mul(usize_to_u64(size_of::<&ResolutionCandidate>())),
    )?;
    targets
        .try_reserve_exact(additional)
        .map_err(|_| StageItemFailure)
}

fn append_named_framework_bridges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    for (name, candidates) in ordered_resolution_candidates(index) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if name.contains("::") {
            continue;
        }
        append_tagged_bridges(
            FrameworkCandidateMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                candidates,
                cancelled: &mut *cancelled,
            },
            TaggedBridgeRule {
                source_tag: "::native-module-spec::",
                target_tags: &["::react-native-module::", "::expo-module::"],
                provenance: NATIVE_MODULE_BRIDGE_PROVENANCE,
            },
        )?;
        append_tagged_bridges(
            FrameworkCandidateMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                candidates,
                cancelled: &mut *cancelled,
            },
            TaggedBridgeRule {
                source_tag: "::fabric-component::",
                target_tags: &["::native-view-manager::"],
                provenance: FABRIC_NATIVE_BRIDGE_PROVENANCE,
            },
        )?;
        append_turbo_method_bridges(FrameworkCandidateMutation {
            index,
            facts: &mut *facts,
            budget: &mut *budget,
            candidates,
            cancelled: &mut *cancelled,
        })?;
        append_drupal_tag_bridges(FrameworkCandidateMutation {
            index,
            facts: &mut *facts,
            budget: &mut *budget,
            candidates,
            cancelled: &mut *cancelled,
        })?;
        append_native_event_bridges(FrameworkCandidateMutation {
            index,
            facts: &mut *facts,
            budget: &mut *budget,
            candidates,
            cancelled: &mut *cancelled,
        })?;
    }
    Ok(())
}

fn append_native_event_bridges<Cancel>(
    input: FrameworkCandidateMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let FrameworkCandidateMutation {
        facts,
        budget,
        candidates,
        cancelled,
        ..
    } = input;
    for producer in candidates.iter().filter(|candidate| {
        candidate.kind == SymbolKind::Resource
            && candidate
                .qualified_name
                .contains("::react-native-event-producer::")
    }) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        for consumer in candidates.iter().filter(|candidate| {
            candidate.kind == SymbolKind::Resource
                && candidate
                    .qualified_name
                    .contains("::react-native-event-consumer::")
        }) {
            append_framework_edge(
                facts,
                budget,
                FrameworkEdgeInput {
                    source: producer,
                    target: consumer,
                    confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
                    provenance: NATIVE_EVENT_BRIDGE_PROVENANCE,
                },
            )?;
        }
    }
    Ok(())
}

fn append_manifest_workspace_edges<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    for (qualified_name, workspace_candidates) in ordered_resolution_candidates(index) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        append_manifest_workspace(
            ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            qualified_name,
            workspace_candidates,
        )?;
    }
    Ok(())
}

fn append_manifest_workspace<Cancel>(
    input: ResolutionMutation<'_, Cancel>,
    qualified_name: &str,
    workspace_candidates: &[ResolutionCandidate],
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = input;
    let Some(workspace) = manifest_workspace_tag(qualified_name, false) else {
        return Ok(());
    };
    let Ok(glob) = GlobBuilder::new(workspace.pattern)
        .literal_separator(true)
        .build()
    else {
        return Ok(());
    };
    let matcher = glob.compile_matcher();
    for source in workspace_candidates
        .iter()
        .filter(|candidate| candidate.qualified_name == qualified_name)
        .filter(|candidate| candidate.kind == SymbolKind::Resource)
    {
        append_manifest_workspace_source(ManifestWorkspaceSourceInput {
            mutation: ResolutionMutation {
                index,
                facts: &mut *facts,
                budget: &mut *budget,
                cancelled: &mut *cancelled,
            },
            workspace,
            matcher: &matcher,
            source,
        })?;
    }
    Ok(())
}

struct ManifestWorkspaceSourceInput<'context, 'name, Cancel> {
    mutation: ResolutionMutation<'context, Cancel>,
    workspace: ManifestWorkspaceTag<'name>,
    matcher: &'context globset::GlobMatcher,
    source: &'context ResolutionCandidate,
}

fn append_manifest_workspace_source<Cancel>(
    input: ManifestWorkspaceSourceInput<'_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ManifestWorkspaceSourceInput {
        mutation,
        workspace,
        matcher,
        source,
    } = input;
    let ResolutionMutation {
        index,
        facts,
        budget,
        cancelled,
    } = mutation;
    for (qualified_name, candidates) in ordered_resolution_candidates(index) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(package) = manifest_package_tag(qualified_name) else {
            continue;
        };
        if workspace.ecosystem != package.ecosystem || !matcher.is_match(package.directory) {
            continue;
        }
        if manifest_workspace_excludes(ManifestWorkspaceExclusion {
            index,
            workspace,
            package_directory: package.directory,
            cancelled,
        })? {
            continue;
        }
        append_manifest_resource_targets(
            facts,
            budget,
            ManifestResourceTargets {
                source,
                qualified_name,
                candidates,
            },
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ManifestResourceTargets<'candidate> {
    source: &'candidate ResolutionCandidate,
    qualified_name: &'candidate str,
    candidates: &'candidate [ResolutionCandidate],
}

fn append_manifest_resource_targets(
    facts: &mut GenerationFacts,
    budget: &mut ResolveBudget,
    targets: ManifestResourceTargets<'_>,
) -> Result<(), StageItemFailure> {
    for target in targets
        .candidates
        .iter()
        .filter(|candidate| candidate.qualified_name == targets.qualified_name)
        .filter(|candidate| candidate.kind == SymbolKind::Resource)
    {
        append_framework_edge(
            facts,
            budget,
            FrameworkEdgeInput {
                source: targets.source,
                target,
                confidence: EXACT_PROJECT_CONFIDENCE,
                provenance: MANIFEST_WORKSPACE_PROVENANCE,
            },
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ManifestWorkspaceTag<'name> {
    ecosystem: &'static str,
    root: &'name str,
    pattern: &'name str,
}

#[derive(Clone, Copy)]
struct ManifestPackageTag<'name> {
    ecosystem: &'static str,
    name: &'name str,
    directory: &'name str,
}

fn manifest_workspace_tag(
    qualified_name: &str,
    exclusion: bool,
) -> Option<ManifestWorkspaceTag<'_>> {
    let role = if exclusion { "exclude" } else { "member" };
    for ecosystem in ["npm", "cargo"] {
        let marker = format!("::manifest-workspace-{role}-{ecosystem}::");
        let Some((_, suffix)) = qualified_name.split_once(&marker) else {
            continue;
        };
        let (root, pattern) = suffix.split_once("::pattern::")?;
        if !root.is_empty() && !pattern.is_empty() {
            return Some(ManifestWorkspaceTag {
                ecosystem,
                root,
                pattern,
            });
        }
    }
    None
}

fn manifest_package_tag(qualified_name: &str) -> Option<ManifestPackageTag<'_>> {
    for ecosystem in ["npm", "cargo"] {
        let marker = format!("::manifest-package-{ecosystem}::");
        let Some((_, suffix)) = qualified_name.split_once(&marker) else {
            continue;
        };
        let (name, directory) = suffix.split_once("::manifest-dir::")?;
        if !name.is_empty() && !directory.is_empty() {
            return Some(ManifestPackageTag {
                ecosystem,
                name,
                directory,
            });
        }
    }
    None
}

fn manifest_workspace_excludes<Cancel>(
    input: ManifestWorkspaceExclusion<'_, Cancel>,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ManifestWorkspaceExclusion {
        index,
        workspace,
        package_directory,
        cancelled,
    } = input;
    for (qualified_name, _) in ordered_resolution_candidates(index) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(exclusion) = manifest_workspace_tag(qualified_name, true) else {
            continue;
        };
        if exclusion.ecosystem != workspace.ecosystem || exclusion.root != workspace.root {
            continue;
        }
        let Ok(glob) = GlobBuilder::new(exclusion.pattern)
            .literal_separator(true)
            .build()
        else {
            continue;
        };
        if glob.compile_matcher().is_match(package_directory) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn append_drupal_tag_bridges<Cancel>(
    input: FrameworkCandidateMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    drupal_tags::append(input)
}

fn drupal_tag_role(candidate: &ResolutionCandidate) -> Option<(bool, &str)> {
    for (tag, provider) in [
        ("::drupal-tag-provider::", true),
        ("::drupal-tag-consumer::", false),
    ] {
        let Some((_, suffix)) = candidate.qualified_name.split_once(tag) else {
            continue;
        };
        let (_, service_id) = suffix.split_once("::")?;
        if !service_id.is_empty() {
            return Some((provider, service_id));
        }
    }
    None
}

fn unique_drupal_service<Cancel>(
    input: DrupalServiceQuery<'_, Cancel>,
) -> Result<Option<&ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let DrupalServiceQuery {
        index,
        file_id,
        service_id,
        cancelled,
    } = input;
    let candidates = resolution_candidates_for_file(index, service_id, file_id);
    select_candidate(
        candidates,
        |candidate| {
            &candidate.file_id == file_id
                && candidate.kind == SymbolKind::Resource
                && candidate.qualified_name.contains("::drupal-service::")
        },
        cancelled,
    )
}

fn append_turbo_method_bridges<Cancel>(
    input: FrameworkCandidateMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let FrameworkCandidateMutation {
        index,
        facts,
        budget,
        candidates,
        cancelled,
    } = input;
    for source in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(source_file) = index.modules.files.get(&source.file_id) else {
            return Err(StageItemFailure);
        };
        let Some(module) =
            tagged_framework_module(&source.qualified_name, "::turbo-module-spec-method::")
        else {
            continue;
        };
        if !javascript_family_name(&source_file.language) {
            continue;
        }
        if let Some(target) = select_turbo_native_target(TurboTargetQuery {
            index,
            candidates,
            module,
            cancelled,
        })? {
            let target = objc_alias_method(index, target).unwrap_or(target);
            append_framework_edge(
                facts,
                budget,
                FrameworkEdgeInput {
                    source,
                    target,
                    confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
                    provenance: TURBO_NATIVE_BRIDGE_PROVENANCE,
                },
            )?;
        }
    }
    Ok(())
}

fn select_turbo_native_target<'candidate, Cancel>(
    input: TurboTargetQuery<'_, 'candidate, Cancel>,
) -> Result<Option<&'candidate ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let TurboTargetQuery {
        index,
        candidates,
        module,
        cancelled,
    } = input;
    for languages in [&["objc"][..], &["java", "kotlin"][..]] {
        let tagged = select_turbo_target_tier(TurboTargetTierQuery {
            index,
            candidates,
            module,
            languages,
            tagged: true,
            cancelled,
        })?;
        match tagged {
            CandidateTier::Unique(candidate) => return Ok(Some(candidate)),
            CandidateTier::Ambiguous => return Ok(None),
            CandidateTier::Missing => {}
        }
        let structural = select_turbo_target_tier(TurboTargetTierQuery {
            index,
            candidates,
            module,
            languages,
            tagged: false,
            cancelled,
        })?;
        match structural {
            CandidateTier::Unique(candidate) => return Ok(Some(candidate)),
            CandidateTier::Ambiguous => return Ok(None),
            CandidateTier::Missing => {}
        }
    }
    Ok(None)
}

enum CandidateTier<'candidate> {
    Missing,
    Unique(&'candidate ResolutionCandidate),
    Ambiguous,
}

fn select_turbo_target_tier<'candidate, Cancel>(
    input: TurboTargetTierQuery<'_, 'candidate, Cancel>,
) -> Result<CandidateTier<'candidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let TurboTargetTierQuery {
        index,
        candidates,
        module,
        languages,
        tagged,
        cancelled,
    } = input;
    let mut eligible = 0_usize;
    let selected = select_candidate(
        candidates,
        |candidate| {
            let Some(file) = index.modules.files.get(&candidate.file_id) else {
                return false;
            };
            let is_tagged = candidate.qualified_name.contains("::react-native-method::");
            let matches = candidate.kind == SymbolKind::Method
                && languages.contains(&file.language.as_str())
                && is_tagged == tagged
                && (tagged
                    || !framework_synthetic_candidate(candidate)
                    || objc_keyword_selector_alias(candidate).is_some())
                && turbo_module_matches(module, file, candidate);
            if matches {
                eligible = eligible.saturating_add(1);
            }
            matches
        },
        cancelled,
    )?;
    Ok(match (selected, eligible) {
        (Some(candidate), _) => CandidateTier::Unique(candidate),
        (None, 0) => CandidateTier::Missing,
        (None, _) => CandidateTier::Ambiguous,
    })
}

/// The keyword selector (`setValue:forKey:`) of an Objective-C method's literal
/// Swift alias (`setValue`), the name a `TurboModule` spec method shares with its
/// native implementation. Unary selectors and preposition-reduced aliases are
/// not literal aliases: a unary method already shares its own name.
fn objc_keyword_selector_alias(candidate: &ResolutionCandidate) -> Option<&str> {
    let (_, tail) = candidate
        .qualified_name
        .split_once("::objc-swift-method::")?;
    let (selector, alias) = tail.rsplit_once("::")?;
    let alias = alias.split_once('#').map_or(alias, |(alias, _)| alias);
    let (head, _) = selector.split_once(':')?;
    (head == alias).then_some(selector)
}

/// The native method a literal selector alias stands for.
fn objc_alias_method<'index>(
    index: &'index ResolutionIndex,
    alias: &ResolutionCandidate,
) -> Option<&'index ResolutionCandidate> {
    let selector = objc_keyword_selector_alias(alias)?;
    let method = index.parents.get(&alias.symbol_id)?;
    index
        .candidates
        .get(selector)?
        .as_slice()
        .iter()
        .find(|candidate| &candidate.symbol_id == method)
}

/// Synthetic framework or bridge landmarks, including JS-visible React Native
/// methods, which are never a source declaration's own member.
fn framework_landmark_candidate(candidate: &ResolutionCandidate) -> bool {
    framework_synthetic_candidate(candidate)
        || candidate.qualified_name.contains("::react-native-method::")
}

fn framework_synthetic_candidate(candidate: &ResolutionCandidate) -> bool {
    [
        "::objc-swift-method::",
        "::swift-objc-method::",
        "::expo-module-method::",
        "::turbo-module-spec-method::",
    ]
    .iter()
    .any(|tag| candidate.qualified_name.contains(tag))
}

fn tagged_framework_module<'name>(qualified_name: &'name str, tag: &str) -> Option<&'name str> {
    let (_, suffix) = qualified_name.split_once(tag)?;
    let (module, _) = suffix.split_once("::")?;
    (!module.is_empty()).then_some(module)
}

fn turbo_module_matches(
    module: &str,
    file: &ResolutionFileContext,
    candidate: &ResolutionCandidate,
) -> bool {
    if tagged_framework_module(&candidate.qualified_name, "::react-native-method::")
        .is_some_and(|candidate_module| candidate_module == module)
    {
        return true;
    }
    module.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && (normalized_framework_contains(&file.path, module)
            || normalized_framework_contains(&candidate.qualified_name, module))
}

fn normalized_framework_contains(value: &str, needle: &str) -> bool {
    for start in 0..value.len() {
        if !value.as_bytes()[start].is_ascii_alphanumeric() {
            continue;
        }
        let mut candidate = value.as_bytes()[start..]
            .iter()
            .copied()
            .filter(u8::is_ascii_alphanumeric)
            .map(|byte| byte.to_ascii_lowercase());
        let matches = needle
            .bytes()
            .filter(u8::is_ascii_alphanumeric)
            .map(|byte| byte.to_ascii_lowercase())
            .all(|byte| candidate.next() == Some(byte));
        if matches {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy)]
struct TaggedBridgeRule {
    source_tag: &'static str,
    target_tags: &'static [&'static str],
    provenance: &'static str,
}

fn append_tagged_bridges<Cancel>(
    input: FrameworkCandidateMutation<'_, Cancel>,
    rule: TaggedBridgeRule,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let FrameworkCandidateMutation {
        index,
        facts,
        budget,
        candidates,
        cancelled,
    } = input;
    let target = unique_tagged_candidate(
        TaggedCandidateQuery { index, candidates },
        |language, candidate| {
            native_bridge_target_language(language)
                && rule
                    .target_tags
                    .iter()
                    .any(|tag| candidate.qualified_name.contains(tag))
        },
        cancelled,
    )?;
    let Some(target) = target else {
        return Ok(());
    };
    for source in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(file) = index.modules.files.get(&source.file_id) else {
            return Err(StageItemFailure);
        };
        if javascript_family_name(&file.language) && source.qualified_name.contains(rule.source_tag)
        {
            append_framework_edge(
                facts,
                budget,
                FrameworkEdgeInput {
                    source,
                    target,
                    confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
                    provenance: rule.provenance,
                },
            )?;
        }
    }
    Ok(())
}

fn unique_tagged_candidate<'candidate, Predicate, Cancel>(
    query: TaggedCandidateQuery<'_, 'candidate>,
    mut predicate: Predicate,
    cancelled: &mut Cancel,
) -> Result<Option<&'candidate ResolutionCandidate>, StageItemFailure>
where
    Predicate: FnMut(&str, &ResolutionCandidate) -> bool,
    Cancel: FnMut() -> bool,
{
    let TaggedCandidateQuery { index, candidates } = query;
    let mut unique = None;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(file) = index.modules.files.get(&candidate.file_id) else {
            return Err(StageItemFailure);
        };
        if !predicate(&file.language, candidate) {
            continue;
        }
        if unique.is_some() {
            return Ok(None);
        }
        unique = Some(candidate);
    }
    Ok(unique)
}

fn append_framework_edge(
    facts: &mut GenerationFacts,
    budget: &mut ResolveBudget,
    input: FrameworkEdgeInput<'_>,
) -> Result<(), StageItemFailure> {
    let FrameworkEdgeInput {
        source,
        target,
        confidence,
        provenance,
    } = input;
    append_derived_edge(
        facts,
        budget,
        DerivedEdgeInput {
            source_symbol_id: &source.symbol_id,
            target_symbol_id: &target.symbol_id,
            kind: EdgeKind::References,
            confidence,
            provenance,
        },
    )
}

#[derive(Clone, Copy)]
struct DerivedEdgeInput<'symbol> {
    source_symbol_id: &'symbol SymbolId,
    target_symbol_id: &'symbol SymbolId,
    kind: EdgeKind,
    confidence: f32,
    provenance: &'static str,
}

fn append_derived_edge(
    facts: &mut GenerationFacts,
    budget: &mut ResolveBudget,
    input: DerivedEdgeInput<'_>,
) -> Result<(), StageItemFailure> {
    let DerivedEdgeInput {
        source_symbol_id,
        target_symbol_id,
        kind,
        confidence,
        provenance,
    } = input;
    if source_symbol_id == target_symbol_id {
        return Ok(());
    }
    let retained = usize_to_u64(size_of::<EdgeInput>())
        .saturating_add(usize_to_u64(source_symbol_id.as_str().len()))
        .saturating_add(usize_to_u64(target_symbol_id.as_str().len()))
        .saturating_add(usize_to_u64(provenance.len()));
    budget.charge(retained)?;
    facts.edges.try_reserve(1).map_err(|_| StageItemFailure)?;
    facts.edges.push(EdgeInput {
        source_symbol_id: source_symbol_id.clone(),
        target_symbol_id: target_symbol_id.clone(),
        kind,
        confidence,
        provenance: try_clone_text(provenance)?,
        site_count: 1,
    });
    Ok(())
}

struct ResolutionIndexContext<'context, Cancel> {
    source_root: &'context SourceRoot,
    budget: &'context mut ResolveBudget,
    cancelled: &'context mut Cancel,
}

fn build_resolution_index<Cancel>(
    extracted: &NativeFactAccumulator,
    mut context: ResolutionIndexContext<'_, Cancel>,
) -> Result<ResolutionIndex, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut index = ResolutionIndex::default();
    for file in &extracted.files {
        index_resolution_file_metadata(ResolutionIndexFileInput {
            index: &mut index,
            file,
            budget: context.budget,
            cancelled: context.cancelled,
        })?;
        qualtype_resolution::index_syntax(&mut index, file, &mut context)?;
    }
    index_typescript_aliases(&mut index.modules, extracted, &mut context)?;
    index_rust_workspace_packages(RustWorkspacePackageIndexInput {
        index: &mut index,
        extracted,
        budget: context.budget,
        cancelled: context.cancelled,
    })?;
    for file in &extracted.files {
        index_resolution_file_symbols(ResolutionIndexFileInput {
            index: &mut index,
            file,
            budget: context.budget,
            cancelled: context.cancelled,
        })?;
    }
    finalize_resolution_candidate_order(&mut index, context.budget, context.cancelled)?;
    javascript_exports::prepare(&mut index, context.budget, context.cancelled)?;
    Ok(index)
}

fn finalize_resolution_candidate_order<Cancel>(
    index: &mut ResolutionIndex,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    python_resolution::index_source_roots(&mut ResolutionIndexTarget { index, budget }, cancelled)?;
    receiver_resolution::finish_index(&mut ResolutionIndexTarget { index, budget }, cancelled)?;
    ocaml_module_resolution::finalize(&mut ResolutionIndexTarget { index, budget }, cancelled)?;
    shell_resolution::finalize(&mut ResolutionIndexTarget { index, budget }, cancelled)?;
    rust_root_ownership::index(&mut ResolutionIndexTarget { index, budget }, cancelled)?;
    rust_path_guards::finalize(&mut ResolutionIndexTarget { index, budget }, cancelled)?;
    rust_facade_resolution::index(&mut ResolutionIndexTarget { index, budget }, cancelled)?;
    let mut order = Vec::new();
    order
        .try_reserve_exact(index.candidates.len())
        .map_err(|_| StageItemFailure)?;
    for key in index.candidates.keys() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        budget.charge(usize_to_u64(size_of::<String>()).saturating_add(usize_to_u64(key.len())))?;
        order.push(try_clone_text(key)?);
    }
    order.sort_unstable();
    index.candidate_order = order;
    jvm_resolution::prepare_wildcards(&mut ResolutionIndexTarget { index, budget }, cancelled)
}

fn ordered_resolution_candidates(
    index: &ResolutionIndex,
) -> impl Iterator<Item = (&str, &[ResolutionCandidate])> {
    index.candidate_order.iter().filter_map(|key| {
        index
            .candidates
            .get(key)
            .map(|candidates| (key.as_str(), candidates.as_slice()))
    })
}

fn resolution_candidates_for_file<'a>(
    index: &'a ResolutionIndex,
    name: &str,
    file_id: &FileId,
) -> &'a [ResolutionCandidate] {
    let Some(file_ordinal) = index.file_ordinals.get(file_id) else {
        return &[];
    };
    index
        .candidates
        .get(name)
        .map_or(&[] as &[ResolutionCandidate], |candidates| {
            candidates.for_file(*file_ordinal)
        })
}

struct ProjectResolutionCandidates<'a> {
    bucket: &'a ResolutionCandidateBucket,
    globally_visible_position: usize,
    languages: [Option<&'a str>; MAXIMUM_PROJECT_NON_VISIBLE_LANGUAGES],
    language_count: usize,
    language_position: usize,
    candidate_position: usize,
}

impl<'a> ProjectResolutionCandidates<'a> {
    fn new(
        bucket: &'a ResolutionCandidateBucket,
        source: &'a ResolutionFileContext,
        reference_name: &str,
    ) -> Result<Self, StageItemFailure> {
        let mut candidates = Self {
            bucket,
            globally_visible_position: 0,
            languages: [None; MAXIMUM_PROJECT_NON_VISIBLE_LANGUAGES],
            language_count: 0,
            language_position: 0,
            candidate_position: 0,
        };
        candidates.push_language(SourceLanguage::Properties.as_str())?;
        candidates.push_bridge_languages(&source.language)?;
        if same_language_framework_reference(source, reference_name) {
            if javascript_family_name(&source.language) {
                for language in ["javascript", "jsx", "typescript", "tsx"] {
                    candidates.push_language(language)?;
                }
            } else {
                candidates.push_language(&source.language)?;
            }
        }
        if php_route_source(source) {
            candidates.push_language(SourceLanguage::Php.as_str())?;
        }
        Ok(candidates)
    }

    fn push_bridge_languages(&mut self, source_language: &'a str) -> Result<(), StageItemFailure> {
        let languages: &[&str] = if javascript_family_name(source_language) {
            &["apex", "objc", "swift", "java", "kotlin"]
        } else {
            match source_language {
                "svelte" | "vue" | "astro" | "html" => &["javascript", "jsx", "typescript", "tsx"],
                "aura" | "visualforce" => &["apex"],
                "apex" => &["aura", "visualforce"],
                "xml" => &["java", "kotlin", "scala"],
                "java" | "kotlin" | "scala" => &["xml"],
                "swift" => &["objc"],
                "objc" => &["swift"],
                "rust" => &["rust"],
                _ => &[],
            }
        };
        for language in languages {
            self.push_language(language)?;
        }
        Ok(())
    }

    fn push_language(&mut self, language: &'a str) -> Result<(), StageItemFailure> {
        if self.languages[..self.language_count]
            .iter()
            .flatten()
            .any(|retained| *retained == language)
        {
            return Ok(());
        }
        let slot = self
            .languages
            .get_mut(self.language_count)
            .ok_or(StageItemFailure)?;
        *slot = Some(language);
        self.language_count = self.language_count.checked_add(1).ok_or(StageItemFailure)?;
        Ok(())
    }
}

impl<'a> Iterator for ProjectResolutionCandidates<'a> {
    type Item = &'a ResolutionCandidate;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(index) = self
            .bucket
            .globally_visible
            .get(self.globally_visible_position)
        {
            self.globally_visible_position = self.globally_visible_position.saturating_add(1);
            return self.bucket.candidates.get(*index);
        }
        while self.language_position < self.language_count {
            let language = self.languages[self.language_position]?;
            let indices = self
                .bucket
                .non_visible_by_language
                .get(language)
                .map_or(&[] as &[usize], Vec::as_slice);
            if let Some(index) = indices.get(self.candidate_position) {
                self.candidate_position = self.candidate_position.saturating_add(1);
                return self.bucket.candidates.get(*index);
            }
            self.language_position = self.language_position.saturating_add(1);
            self.candidate_position = 0;
        }
        None
    }
}

fn same_language_framework_reference(source: &ResolutionFileContext, reference_name: &str) -> bool {
    match source.language.as_str() {
        "javascript" | "jsx" | "typescript" | "tsx" => {
            JAVASCRIPT_FRAMEWORK_RULES
                .iter()
                .any(|rule| rule.name.matches(reference_name))
                || framework_pascal_case(reference_name)
        }
        "svelte" | "vue" | "astro" | "html" => true,
        "java" | "kotlin" | "scala" => JVM_FRAMEWORK_RULES
            .iter()
            .any(|rule| rule.name.matches(reference_name)),
        "ruby" => framework_pascal_case(reference_name),
        "php" => {
            PHP_FRAMEWORK_RULES
                .iter()
                .any(|rule| rule.name.matches(reference_name))
                || php_route_source(source)
        }
        "csharp" => CSHARP_FRAMEWORK_RULES
            .iter()
            .any(|rule| rule.name.matches(reference_name)),
        "python" => PYTHON_FRAMEWORK_RULES
            .iter()
            .any(|rule| rule.name.matches(reference_name)),
        "swift" => SWIFT_FRAMEWORK_RULES
            .iter()
            .any(|rule| rule.name.matches(reference_name)),
        _ => framework_conventions::reference(source, reference_name),
    }
}

fn index_typescript_aliases<Cancel>(
    modules: &mut ModulePathIndex,
    extracted: &NativeFactAccumulator,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for file in &extracted.files {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        index_typescript_alias_file(modules, file, context)?;
    }
    javascript_config::finish(modules, context)
}

fn index_typescript_alias_file<Cancel>(
    modules: &mut ModulePathIndex,
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    javascript_config::index_file(modules, file, context)
}

fn parse_typescript_path_mapping(
    pattern: &str,
    substitutions: &Value,
) -> Result<Option<TypeScriptPathMapping>, StageItemFailure> {
    if !valid_typescript_alias_text(pattern) {
        return Ok(None);
    }
    let Some(substitutions) = substitutions.as_array() else {
        return Ok(None);
    };
    if substitutions.len() > MAXIMUM_TYPESCRIPT_PATH_SUBSTITUTIONS {
        return Err(StageItemFailure);
    }
    let mut retained_substitutions = Vec::new();
    retained_substitutions
        .try_reserve_exact(substitutions.len())
        .map_err(|_| StageItemFailure)?;
    for substitution in substitutions.iter().filter_map(Value::as_str) {
        if valid_typescript_alias_substitution(substitution) {
            retained_substitutions.push(try_clone_text(substitution)?);
        }
    }
    if retained_substitutions.is_empty() {
        return Ok(None);
    }
    Ok(Some(TypeScriptPathMapping {
        pattern: try_clone_text(pattern)?,
        substitutions: retained_substitutions,
    }))
}

fn normalize_typescript_alias_base(directory: &str, base_url: &str) -> Option<String> {
    javascript_config::normalize_base(directory, base_url)
}

fn valid_typescript_alias_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_TYPESCRIPT_PATH_TEXT_BYTES
        && !value.contains(['\\', '\0'])
        && value.bytes().filter(|byte| *byte == b'*').count() <= 1
}

fn valid_typescript_alias_substitution(value: &str) -> bool {
    valid_typescript_alias_text(value) && !value.starts_with('/')
}

fn typescript_alias_config_bytes(directory: &str, config: &TypeScriptAliasConfig) -> u64 {
    let mut bytes = RESOLUTION_MAP_NODE_ALLOWANCE
        .saturating_add(usize_to_u64(directory.len()))
        .saturating_add(usize_to_u64(config.base_path.capacity()))
        .saturating_add(
            config
                .explicit_base_url
                .as_ref()
                .map_or(0, |base| usize_to_u64(base.capacity())),
        )
        .saturating_add(vector_capacity_bytes(&config.mappings));
    for mapping in &config.mappings {
        bytes = bytes
            .saturating_add(usize_to_u64(mapping.pattern.capacity()))
            .saturating_add(vector_capacity_bytes(&mapping.substitutions));
        for substitution in &mapping.substitutions {
            bytes = bytes.saturating_add(usize_to_u64(substitution.capacity()));
        }
    }
    bytes
}

#[cfg(test)]
fn strip_typescript_config_comments(source: &str) -> String {
    javascript_config::strip_typescript_config_comments(source)
}

fn copy_typescript_config_quoted_byte(
    bytes: &[u8],
    index: usize,
    output: &mut Vec<u8>,
) -> (usize, bool) {
    let byte = bytes[index];
    output.push(byte);
    match (byte, bytes.get(index + 1).copied()) {
        (b'\\', Some(escaped)) => {
            output.push(escaped);
            (index + 2, true)
        }
        (b'"', _) => (index + 1, false),
        _ => (index + 1, true),
    }
}

fn skip_typescript_line_comment(bytes: &[u8], index: usize) -> usize {
    bytes[index + 2..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| index + 2 + offset)
}

fn skip_typescript_block_comment(bytes: &[u8], index: usize) -> usize {
    bytes[index + 2..]
        .windows(2)
        .position(|window| window == b"*/")
        .map_or(bytes.len(), |offset| index + 2 + offset + 2)
}

struct ResolutionIndexFileInput<'index, 'file, 'budget, 'cancel, Cancel> {
    index: &'index mut ResolutionIndex,
    file: &'file NativeFileFacts,
    budget: &'budget mut ResolveBudget,
    cancelled: &'cancel mut Cancel,
}

struct RustWorkspacePackageIndexInput<'index, 'facts, 'budget, 'cancel, Cancel> {
    index: &'index mut ResolutionIndex,
    extracted: &'facts NativeFactAccumulator,
    budget: &'budget mut ResolveBudget,
    cancelled: &'cancel mut Cancel,
}

struct RustWorkspacePackageInsertInput<'packages, 'budget> {
    packages: &'packages mut RustPackageMap,
    crate_name: String,
    root: RustPackageRoot,
    budget: &'budget mut ResolveBudget,
}

fn index_resolution_file_metadata<Cancel>(
    mut input: ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    javascript_member_resolution::index_file(&mut input)?;
    javascript_framework_resolution::index_file(&mut input)?;
    let ResolutionIndexFileInput {
        index,
        file,
        budget,
        cancelled,
    } = input;
    if cancelled() {
        return Err(StageItemFailure);
    }
    index_file_ordinal(&mut index.file_ordinals, &file.file, budget)?;
    index_file_symbol(&mut index.file_symbols, &file.file, budget)?;
    index_module_path(
        &mut index.modules,
        ModuleFileIndexInput {
            file: &file.file,
            package: native_package_name(file),
        },
        budget,
    )?;
    file_path_resolution::index_path(
        &mut index.modules.file_paths,
        &file.file,
        (budget, cancelled),
    )?;
    index_test_file_evidence(&mut index.test_files, file, budget)?;
    for containment in &file.containments {
        if cancelled() {
            return Err(StageItemFailure);
        }
        insert_parent(&mut index.parents, containment, budget)?;
    }
    generic_resolution::index_calls(
        &mut index.generic,
        (&file.call_scope_sites, budget),
        cancelled,
    )?;
    Ok(())
}

fn index_rust_workspace_packages<Cancel>(
    input: RustWorkspacePackageIndexInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let RustWorkspacePackageIndexInput {
        index,
        extracted,
        budget,
        cancelled,
    } = input;
    for file in &extracted.files {
        index_rust_workspace_package_file(
            &mut ResolutionIndexTarget { index, budget },
            file,
            cancelled,
        )?;
        rust_dependency_paths::index_file(
            &mut ResolutionIndexTarget { index, budget },
            file,
            cancelled,
        )?;
    }
    Ok(())
}

fn index_rust_workspace_package_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionIndexTarget { index, budget } = target;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some((crate_name, root)) =
            rust_workspace_package_root(&index.modules, &symbol.input.qualified_name)?
        else {
            continue;
        };
        insert_rust_workspace_package(RustWorkspacePackageInsertInput {
            packages: &mut index.modules.rust_packages,
            crate_name,
            root,
            budget,
        })?;
    }
    Ok(())
}

fn rust_workspace_package_root(
    modules: &ModulePathIndex,
    qualified_name: &str,
) -> Result<Option<(String, RustPackageRoot)>, StageItemFailure> {
    let Some(package) =
        manifest_package_tag(qualified_name).filter(|package| package.ecosystem == "cargo")
    else {
        return Ok(None);
    };
    let Some(crate_name) = rust_crate_identifier(package.name) else {
        return Ok(None);
    };
    let directory = if package.directory == "." {
        ""
    } else {
        package.directory
    };
    let entry_path = joined_path(directory, "src/lib.rs")?;
    let ModuleFileMatch::Unique(entry_file_id) = module_file_match(
        modules.exact.get(&entry_path),
        &modules.files,
        SourceLanguage::Rust.as_str(),
    ) else {
        return Ok(None);
    };
    Ok(Some((
        crate_name,
        RustPackageRoot {
            directory: try_clone_text(directory)?,
            entry_file_id: entry_file_id.clone(),
        },
    )))
}

fn insert_rust_workspace_package(
    input: RustWorkspacePackageInsertInput<'_, '_>,
) -> Result<(), StageItemFailure> {
    let RustWorkspacePackageInsertInput {
        packages,
        crate_name,
        root,
        budget,
    } = input;
    if let Some(existing) = packages.get_mut(&crate_name) {
        if existing.as_ref().is_none_or(|existing| {
            existing.directory != root.directory || existing.entry_file_id != root.entry_file_id
        }) {
            *existing = None;
        }
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(String, Option<RustPackageRoot>)>()))
            .saturating_add(usize_to_u64(crate_name.len()))
            .saturating_add(usize_to_u64(root.directory.len()))
            .saturating_add(usize_to_u64(root.entry_file_id.as_str().len())),
    )?;
    packages.insert(crate_name, Some(root));
    Ok(())
}

fn rust_crate_identifier(package_name: &str) -> Option<String> {
    if package_name.is_empty()
        || package_name
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return None;
    }
    Some(package_name.replace('-', "_"))
}

fn index_resolution_file_symbols<Cancel>(
    mut input: ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    javascript_member_resolution::index_static_members(&mut input)?;
    let ResolutionIndexFileInput {
        index,
        file,
        budget,
        cancelled,
    } = input;
    if cancelled() {
        return Err(StageItemFailure);
    }
    qualtype_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    shell_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    rust_path_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    rust_path_guards::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    module_call_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    go_module_paths::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    ocaml_module_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    let file_ordinal = *index
        .file_ordinals
        .get(&file.file.file_id)
        .ok_or(StageItemFailure)?;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.input.symbol_kind == "import" {
            continue;
        }
        index_resolution_symbol(index, (file, file_ordinal, symbol), budget)?;
    }
    rust_scoped_modules::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    index_resolution_file_families(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )
}

/// Index one non-import symbol for candidate, ownership and family lookups.
fn index_resolution_symbol(
    index: &mut ResolutionIndex,
    (file, file_ordinal, symbol): (&NativeFileFacts, u64, &NativeSymbolFacts),
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    qualified_member_resolution::index_type(&mut index.types, symbol, budget)?;
    let parent_symbol_id = index.parents.get(&symbol.input.symbol_id).cloned();
    index_compilation_unit(
        &mut index.modules,
        CompilationUnitIndexInput {
            language: file.file.language.as_str(),
            symbol,
            top_level: parent_symbol_id.is_none(),
        },
        budget,
    )?;
    let visibility = javascript_member_resolution::candidate_visibility(index, symbol);
    let insertion = ResolutionCandidateInsertion {
        key: &symbol.name,
        symbol,
        parent_symbol_id: parent_symbol_id.as_ref(),
        file_ordinal,
        language: &file.file.language,
        visibility,
    };
    push_symbol_candidates(&mut index.candidates, insertion, budget)?;
    framework_methods::index_method(
        &mut index.framework_methods,
        framework_methods::MethodInput {
            symbol,
            parent: parent_symbol_id.as_ref(),
            language: &file.file.language,
        },
        budget,
    )?;
    generic_resolution::index_symbol(&mut index.generic, insertion, budget)?;
    if symbol.export.default_export {
        push_default_export(
            &mut index.default_exports,
            DefaultExportInsertion {
                symbol,
                parent_symbol_id: parent_symbol_id.as_ref(),
            },
            budget,
        )?;
    }
    if symbol.export.exported
        && parent_symbol_id.is_none()
        && symbol.input.qualified_name == symbol.name
    {
        index_project_export(&mut index.exports, symbol, budget)?;
    }
    Ok(())
}

/// Per-file framework, PHP, JVM, receiver and re-export indexes.
fn index_resolution_file_families<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ResolutionIndexTarget { index, budget } = target;
    drupal_tags::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    drupal_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    javascript_alias_exports::index_file(&mut index.javascript_aliases, file, (budget, cancelled))?;
    salesforce_resolution::index_file(
        salesforce_resolution::SalesforceFileInput {
            index: &mut index.salesforce,
            file,
            budget,
        },
        cancelled,
    )?;
    qualified_member_resolution::index_ancestors(index, file, cancelled)?;
    php_resolution::index_file(
        php_resolution::PhpFileIndexInput {
            index: &mut index.php,
            file,
            budget,
        },
        cancelled,
    )?;
    jvm_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    receiver_resolution::index_file(
        &mut ResolutionIndexTarget { index, budget },
        file,
        cancelled,
    )?;
    index_project_reexports(index, file, budget)
}

/// Index one symbol under each distinct name a reference can spell it by:
/// its own name (`insertion.key`), then its qualified name and its framework
/// alias when those differ from the names already indexed.
fn push_symbol_candidates(
    candidates: &mut CandidateMap,
    insertion: ResolutionCandidateInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let name = insertion.key;
    let qualified_name = insertion.symbol.input.qualified_name.as_str();
    let alias = framework_resolution_alias(insertion.symbol, insertion.language)
        .filter(|alias| *alias != name && *alias != qualified_name);
    let jvm_alias = jvm_resolution::candidate_alias(insertion)?;
    let keys = [
        Some(name),
        (qualified_name != name).then_some(qualified_name),
        alias,
        jvm_alias.as_deref(),
    ];
    for key in keys.into_iter().flatten() {
        push_candidate(
            candidates,
            ResolutionCandidateInsertion { key, ..insertion },
            budget,
        )?;
    }
    Ok(())
}

fn index_project_export(
    exports: &mut FileExportMap,
    symbol: &NativeSymbolFacts,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let public_name = if symbol.export.default_export {
        "default"
    } else {
        symbol.name.as_str()
    };
    if !exports.contains_key(&symbol.input.file_id) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(
                    FileId,
                    BTreeMap<String, Option<ProjectExport>>,
                )>()))
                .saturating_add(usize_to_u64(symbol.input.file_id.as_str().len())),
        )?;
        exports.insert(symbol.input.file_id.clone(), BTreeMap::new());
    }
    let by_name = exports
        .get_mut(&symbol.input.file_id)
        .ok_or(StageItemFailure)?;
    if let Some(existing) = by_name.get_mut(public_name) {
        if existing
            .as_ref()
            .is_none_or(|entry| entry.symbol_id != symbol.input.symbol_id)
        {
            *existing = None;
        }
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(public_name.len()))
            .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len())),
    )?;
    by_name.insert(
        try_clone_text(public_name)?,
        Some(ProjectExport {
            symbol_id: symbol.input.symbol_id.clone(),
            default_export: symbol.export.default_export,
        }),
    );
    Ok(())
}

fn index_project_reexports(
    index: &mut ResolutionIndex,
    file: &NativeFileFacts,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    for binding in &file.import_bindings {
        if binding.kind == ImportBindingKind::ReExportUncertain {
            javascript_exports::mark_uncertain(
                &mut index.javascript_exports,
                &file.file.file_id,
                budget,
            )?;
            continue;
        }

        if binding.kind == ImportBindingKind::ReExportNamed {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<RustNamedReExport>()))
                    .saturating_add(usize_to_u64(file.file.file_id.as_str().len()))
                    .saturating_add(usize_to_u64(binding.local_name.len()))
                    .saturating_add(usize_to_u64(binding.module_specifier.len())),
            )?;
            index
                .rust_named_re_exports
                .try_reserve_exact(1)
                .map_err(|_| StageItemFailure)?;
            index.rust_named_re_exports.push(RustNamedReExport {
                source_file_id: file.file.file_id.clone(),
                public_name: try_clone_text(&binding.local_name)?,
                module_specifier: try_clone_text(&binding.module_specifier)?,
            });
            continue;
        }
        let namespace = match binding.kind {
            ImportBindingKind::ReExportAll => false,
            ImportBindingKind::ReExportNamespace => true,
            ImportBindingKind::Default
            | ImportBindingKind::Named
            | ImportBindingKind::Namespace
            | ImportBindingKind::ReExportNamed
            | ImportBindingKind::ReExportUncertain
            | ImportBindingKind::IncludeQuoted
            | ImportBindingKind::IncludeSystem => continue,
        };
        let source_symbol_id = if namespace {
            let mut owners = file.symbols.iter().filter(|symbol| {
                symbol.kind == SymbolKind::Export
                    && symbol.name == binding.local_name
                    && symbol.input.qualified_name == binding.local_name
            });
            let Some(owner) = owners.next() else {
                return Err(StageItemFailure);
            };
            if owners.next().is_some() {
                continue;
            }
            owner.input.symbol_id.clone()
        } else {
            index
                .file_symbols
                .get(&file.file.file_id)
                .cloned()
                .ok_or(StageItemFailure)?
        };
        budget.charge(
            usize_to_u64(size_of::<ProjectReExport>())
                .saturating_add(usize_to_u64(file.file.file_id.as_str().len()))
                .saturating_add(usize_to_u64(source_symbol_id.as_str().len()))
                .saturating_add(usize_to_u64(binding.module_specifier.len())),
        )?;
        index
            .re_exports
            .try_reserve_exact(1)
            .map_err(|_| StageItemFailure)?;
        index.re_exports.push(ProjectReExport {
            source_file_id: file.file.file_id.clone(),
            source_symbol_id,
            module_specifier: try_clone_text(&binding.module_specifier)?,
            namespace,
        });
    }
    Ok(())
}

fn index_test_file_evidence(
    test_files: &mut Vec<TestFileEvidence>,
    file: &NativeFileFacts,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !is_test_source_path(&file.file.normalized_path) && !file.has_inline_tests {
        return Ok(());
    }
    let mut import_specifiers = BTreeSet::new();
    for binding in &file.import_bindings {
        if explicit_edge_resolution::metadata_binding(&file.file.language, binding) {
            continue;
        }
        if !matches!(
            binding.kind,
            ImportBindingKind::IncludeSystem
                | ImportBindingKind::ReExportAll
                | ImportBindingKind::ReExportUncertain
                | ImportBindingKind::ReExportNamespace
        ) {
            import_specifiers.insert(try_clone_text(&binding.module_specifier)?);
        }
    }
    for reference in &file.references {
        if reference.kind == ReferenceKind::Imports {
            import_specifiers.insert(try_clone_text(&reference.name)?);
        }
    }
    let retained = usize_to_u64(size_of::<TestFileEvidence>())
        .saturating_add(usize_to_u64(file.file.file_id.as_str().len()))
        .saturating_add(
            import_specifiers
                .iter()
                .map(|value| usize_to_u64(value.capacity()))
                .fold(0_u64, u64::saturating_add),
        );
    budget.charge(retained)?;
    test_files
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    test_files.push(TestFileEvidence {
        file_id: file.file.file_id.clone(),
        import_specifiers: import_specifiers.into_iter().collect(),
        has_inline_tests: file.has_inline_tests,
    });
    Ok(())
}

fn framework_resolution_alias<'symbol>(
    symbol: &'symbol NativeSymbolFacts,
    language: &str,
) -> Option<&'symbol str> {
    if language == SourceLanguage::Sql.as_str() && symbol.kind == SymbolKind::Table {
        return symbol
            .input
            .qualified_name
            .rsplit_once('.')
            .map(|(_, name)| name)
            .filter(|name| !name.is_empty());
    }
    if symbol.kind != SymbolKind::Method {
        return None;
    }
    for tag in [
        "::react-native-method::",
        "::expo-module-method::",
        "::turbo-module-spec-method::",
    ] {
        if let Some((_, alias)) = symbol.input.qualified_name.split_once(tag)
            && alias.contains("::")
            && !alias.starts_with("::")
            && !alias.ends_with("::")
        {
            return Some(alias);
        }
    }
    if language == SourceLanguage::Php.as_str() {
        let (owner, method) = symbol.input.qualified_name.rsplit_once("::")?;
        let class = owner.rsplit_once("::").map_or(owner, |(_, class)| class);
        if !class.is_empty() && !method.is_empty() {
            let start = owner.len().checked_sub(class.len())?;
            return symbol.input.qualified_name.get(start..);
        }
    }
    None
}

fn index_file_symbol(
    symbols: &mut FileSymbolMap,
    file: &FileInput,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if symbols.contains_key(&file.file_id) {
        return Err(StageItemFailure);
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(FileId, SymbolId)>()))
            .saturating_add(usize_to_u64(file.file_id.as_str().len()))
            .saturating_add(UUID_TEXT_BYTES),
    )?;
    symbols.insert(file.file_id.clone(), native_file_symbol_id(&file.file_id));
    Ok(())
}

fn index_file_ordinal(
    ordinals: &mut FileOrdinalMap,
    file: &FileInput,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if ordinals.contains_key(&file.file_id) {
        return Err(StageItemFailure);
    }
    let ordinal = usize_to_u64(ordinals.len());
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(FileId, u64)>()))
            .saturating_add(usize_to_u64(file.file_id.as_str().len())),
    )?;
    ordinals.insert(file.file_id.clone(), ordinal);
    Ok(())
}

struct ResolutionOutput<'a> {
    index: &'a ResolutionIndex,
    facts: &'a mut GenerationFacts,
    report: &'a mut ResolutionReport,
    budget: &'a mut ResolveBudget,
}

struct FileDocumentIdentity {
    file_id: FileId,
    path: String,
    language: String,
}

struct FileResolutionContext<'a> {
    identity: &'a FileDocumentIdentity,
    file_symbol_id: &'a SymbolId,
    import_bindings: &'a FileImportBindingIndex<'a>,
    current_receivers: &'a generic_resolution::ReceiverSites,
    receiver_lookups: &'a receiver_resolution::FileLookups<'a>,
}

struct FileImportBindingIndex<'a> {
    bindings: &'a [ExtractedImportBinding],
    by_name: HashMap<&'a str, Vec<usize>>,
    wildcards: Vec<usize>,
}

impl<'a> FileImportBindingIndex<'a> {
    fn new(
        bindings: &'a [ExtractedImportBinding],
        budget: &mut ResolveBudget,
        language: &str,
    ) -> Result<Self, StageItemFailure> {
        let mut index = Self {
            bindings,
            by_name: HashMap::new(),
            wildcards: Vec::new(),
        };
        index
            .by_name
            .try_reserve(bindings.len())
            .map_err(|_| StageItemFailure)?;
        index
            .wildcards
            .try_reserve_exact(bindings.len())
            .map_err(|_| StageItemFailure)?;
        for (position, binding) in bindings.iter().enumerate() {
            if binding.module_specifier == cartograph_extract::DRUPAL_CLASS_MODULE {
                continue;
            }
            if salesforce_resolution::implicit_binding(binding, language) {
                continue;
            }
            if php_resolution::implicit_namespace_binding(binding, language) {
                continue;
            }
            if explicit_edge_resolution::metadata_binding(language, binding) {
                continue;
            }
            if binding.local_name == "*" {
                budget.charge(usize_to_u64(size_of::<usize>()))?;
                index.wildcards.push(position);
            } else {
                index.insert(binding.local_name.as_str(), position, budget)?;
            }
            index.insert(binding.imported_name.as_str(), position, budget)?;
            index.insert(binding.module_specifier.as_str(), position, budget)?;
        }
        Ok(index)
    }

    fn insert(
        &mut self,
        name: &'a str,
        position: usize,
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        if name.is_empty() || name == "*" {
            return Ok(());
        }
        if !self.by_name.contains_key(name) {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<(&str, Vec<usize>)>())),
            )?;
            self.by_name.insert(name, Vec::new());
        }
        let positions = self.by_name.get_mut(name).ok_or(StageItemFailure)?;
        if positions.last().copied() == Some(position) {
            return Ok(());
        }
        budget.charge(usize_to_u64(size_of::<usize>()))?;
        positions
            .try_reserve_exact(1)
            .map_err(|_| StageItemFailure)?;
        positions.push(position);
        Ok(())
    }
}

struct ImportBindingScratch {
    positions: Vec<usize>,
    stamps: Vec<u32>,
    generation: u32,
}

impl ImportBindingScratch {
    fn new(bindings: usize, budget: &mut ResolveBudget) -> Result<Self, StageItemFailure> {
        let retained = usize_to_u64(bindings).saturating_mul(usize_to_u64(
            size_of::<usize>().saturating_add(size_of::<u32>()),
        ));
        budget.charge(retained)?;
        let mut positions = Vec::new();
        positions
            .try_reserve_exact(bindings)
            .map_err(|_| StageItemFailure)?;
        let mut stamps = Vec::new();
        stamps
            .try_reserve_exact(bindings)
            .map_err(|_| StageItemFailure)?;
        stamps.resize(bindings, 0);
        Ok(Self {
            positions,
            stamps,
            generation: 0,
        })
    }

    fn select<'a>(
        &'a mut self,
        index: &'a FileImportBindingIndex<'_>,
        reference_name: &str,
    ) -> ImportBindingSelection<'a> {
        self.begin_selection();
        self.extend(index.by_name.get(reference_name));
        for prefix in import_binding_prefixes(reference_name) {
            self.extend(index.by_name.get(prefix));
        }
        self.extend(Some(&index.wildcards));
        self.positions.sort_unstable();
        ImportBindingSelection {
            bindings: index.bindings,
            positions: &self.positions,
            fallback_blocked: false,
        }
    }

    fn begin_selection(&mut self) {
        self.positions.clear();
        if let Some(next) = self.generation.checked_add(1) {
            self.generation = next;
        } else {
            self.stamps.fill(0);
            self.generation = 1;
        }
    }

    fn extend(&mut self, positions: Option<&Vec<usize>>) {
        for position in positions.map_or(&[] as &[usize], Vec::as_slice) {
            let Some(stamp) = self.stamps.get_mut(*position) else {
                continue;
            };
            if *stamp != self.generation {
                *stamp = self.generation;
                self.positions.push(*position);
            }
        }
    }
}

fn import_binding_prefixes(reference_name: &str) -> impl Iterator<Item = &str> {
    reference_name
        .char_indices()
        .filter_map(|(position, character)| {
            let separator = character == '.'
                || character == ':'
                    && reference_name.as_bytes().get(position.saturating_add(1)) == Some(&b':');
            separator.then(|| &reference_name[..position])
        })
        .filter(|prefix| !prefix.is_empty())
}

#[derive(Clone, Copy)]
struct ImportBindingSelection<'a> {
    bindings: &'a [ExtractedImportBinding],
    positions: &'a [usize],
    fallback_blocked: bool,
}

impl<'a> ImportBindingSelection<'a> {
    const fn empty() -> Self {
        Self {
            bindings: &[],
            positions: &[],
            fallback_blocked: false,
        }
    }

    const fn with_fallback_blocked(mut self, blocked: bool) -> Self {
        self.fallback_blocked = blocked;
        self
    }

    fn iter(self) -> impl Iterator<Item = &'a ExtractedImportBinding> {
        self.positions
            .iter()
            .map(|position| &self.bindings[*position])
    }
}

/// Resolution-name classification for one extracted reference.
///
/// A resolver prefix decides which lookup a reference gets, so the prefixes are
/// stripped once here rather than being re-tested through the resolution path.
struct ReferenceLookup<'reference> {
    python_import_fenced: bool,
    dynamic_dispatch_name: Option<&'reference str>,
    rust_self_receiver_name: Option<&'reference str>,
    rust_macro_name: Option<&'reference str>,
    type_query_value_name: Option<&'reference str>,
    lexical_scope_name: Option<&'reference str>,
    embedded_sql: Option<EmbeddedSqlLookup<'reference>>,
    php_exact: Option<php_resolution::PhpExactLookup<'reference>>,
    lookup_name: &'reference str,
}

impl<'reference> ReferenceLookup<'reference> {
    fn classify(reference: &'reference ExtractedReference) -> Self {
        let resolution_name = reference.resolution_name.as_deref();
        let fenced_name = resolution_name
            .and_then(|name| name.strip_prefix(PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX));
        let resolution_name = fenced_name.or(resolution_name);
        let resolution_name = resolution_name
            .and_then(|name| name.strip_prefix(JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX))
            .or(resolution_name);
        let dynamic_dispatch_name =
            resolution_name.and_then(|name| name.strip_prefix(DYNAMIC_DISPATCH_RESOLUTION_PREFIX));
        let rust_self_receiver_name = resolution_name
            .and_then(|name| name.strip_prefix(RUST_SELF_RECEIVER_RESOLUTION_PREFIX));
        let rust_macro_name =
            resolution_name.and_then(|name| name.strip_prefix(RUST_MACRO_RESOLUTION_PREFIX));
        let type_query_value_name =
            resolution_name.and_then(|name| name.strip_prefix(TYPE_QUERY_VALUE_RESOLUTION_PREFIX));
        let lexical_scope_name =
            resolution_name.and_then(|name| name.strip_prefix(LEXICAL_SCOPE_RESOLUTION_PREFIX));
        let embedded_sql = embedded_sql_lookup(resolution_name);
        let php_exact = resolution_name
            .and_then(|name| name.strip_prefix(PHP_EXACT_RESOLUTION_PREFIX))
            .and_then(php_resolution::PhpExactLookup::parse);
        let lookup_name = rust_self_receiver_name
            .or(dynamic_dispatch_name)
            .or(rust_macro_name)
            .or(type_query_value_name)
            .or(lexical_scope_name)
            .or_else(|| embedded_sql.as_ref().map(|lookup| lookup.table))
            .or_else(|| php_exact.map(php_resolution::PhpExactLookup::key))
            .unwrap_or_else(|| resolution_name.unwrap_or(&reference.name));
        Self {
            python_import_fenced: fenced_name.is_some(),
            dynamic_dispatch_name,
            rust_self_receiver_name,
            rust_macro_name,
            type_query_value_name,
            lexical_scope_name,
            embedded_sql,
            php_exact,
            lookup_name,
        }
    }

    /// The receiver dispatch a name-based lookup resolves under.
    const fn dispatch(&self) -> ReferenceDispatch {
        if self.rust_self_receiver_name.is_some() {
            ReferenceDispatch::RustSelf
        } else if self.dynamic_dispatch_name.is_some() {
            ReferenceDispatch::Dynamic
        } else {
            ReferenceDispatch::Static
        }
    }

    /// A type-query value (`typeof value`) reads the value, so it resolves as
    /// a plain reference whatever kind the extractor recorded.
    const fn request_kind(&self, extracted: ReferenceKind) -> ReferenceKind {
        if self.type_query_value_name.is_some() {
            ReferenceKind::References
        } else {
            extracted
        }
    }
}

struct ReferenceAppendRequest<'a, 'b> {
    context: &'a FileResolutionContext<'b>,
    reference: ExtractedReference,
}

struct ExtractedReferenceQuery<'a, 'b> {
    context: &'a FileResolutionContext<'b>,
    reference: &'a ExtractedReference,
    import_binding_scratch: &'a mut ImportBindingScratch,
}

/// Resolve one extracted reference through the lookup its resolver prefix
/// selects, then give a target reached through dynamic dispatch the
/// dynamic-dispatch confidence and provenance.
fn resolve_extracted_reference<Cancel>(
    index: &ResolutionIndex,
    query: ExtractedReferenceQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ExtractedReferenceQuery {
        context,
        reference,
        import_binding_scratch,
    } = query;
    if jvm_resolution::reference_abstains(index, (&context.identity.file_id, reference.span))
        || reference
            .resolution_name
            .as_deref()
            .is_some_and(jvm_resolution::syntax_abstention)
    {
        return Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE));
    }
    let lookup = ReferenceLookup::classify(reference);
    let binding_name =
        javascript_member_resolution::binding_name(index, (&context.identity.file_id, reference))
            .unwrap_or(lookup.lookup_name);
    let mut resolution = if let Some(resolution) =
        resolve_prefixed_reference(index, (context, reference, &lookup), cancelled)?
    {
        resolution
    } else {
        resolve_reference_or_source_name(
            index,
            SourceNameFallbackQuery {
                request: &ResolutionRequest {
                    file_id: &context.identity.file_id,
                    file_path: &context.identity.path,
                    language: &context.identity.language,
                    import_bindings: jvm_resolution::select_import_bindings(
                        (context, &lookup, reference.kind, binding_name),
                        import_binding_scratch,
                    )
                    .with_fallback_blocked(lookup.python_import_fenced),
                    owner: reference.owner.as_ref(),
                    name: lookup.lookup_name,
                    dispatch: lookup.dispatch(),
                    kind: lookup.request_kind(reference.kind),
                    span: reference.span,
                },
                source_name: &reference.name,
            },
            cancelled,
        )?
    };
    if lookup.dynamic_dispatch_name.is_some()
        && let Some(target) = resolution.target.as_mut()
        && target.provenance != generic_resolution::CURRENT_CLASS_PROVENANCE
    {
        target.confidence = DYNAMIC_DISPATCH_CONFIDENCE;
        target.provenance = DYNAMIC_DISPATCH_PROVENANCE;
    }
    let resolution = go_module_paths::prefer(
        index,
        (
            resolution,
            receiver_resolution::ReceiverQuery {
                context,
                reference,
                import_binding_scratch,
            },
        ),
        cancelled,
    )?;
    let receiver = receiver_resolution::resolve(
        index,
        receiver_resolution::ReceiverQuery {
            context,
            reference,
            import_binding_scratch,
        },
        cancelled,
    )?;
    Ok(receiver_resolution::prefer_base(resolution, receiver))
}

/// Honor resolver-prefix routes before the ordinary source-name fallback.
fn resolve_prefixed_reference<Cancel>(
    index: &ResolutionIndex,
    (context, reference, lookup): (
        &FileResolutionContext<'_>,
        &ExtractedReference,
        &ReferenceLookup<'_>,
    ),
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let resolution = if lookup.rust_macro_name.is_some() {
        ReferenceResolution::unresolved(RUST_MACRO_UNRESOLVED_PROVENANCE)
    } else if let Some(sql) = lookup.embedded_sql {
        resolve_embedded_sql(index, sql, cancelled)?
    } else if let Some(name) = lookup.lexical_scope_name {
        resolve_lexical_scope_reference(
            index,
            LexicalScopeReference {
                context,
                reference,
                name,
            },
            cancelled,
        )?
    } else if let Some(php_lookup) = lookup.php_exact {
        php_resolution::resolve_exact(
            index,
            php_resolution::PhpExactRequest {
                file_id: &context.identity.file_id,
                caller_class: php_resolution::caller_class(index, reference.owner.as_ref()),
                lookup: php_lookup,
            },
            cancelled,
        )?
    } else {
        return generic_resolution::resolve_receiver(index, (context, reference), cancelled);
    };
    Ok(Some(resolution))
}

struct FileRecordInput<'file> {
    file: &'file FileInput,
    identity: &'file FileDocumentIdentity,
    file_symbol_id: &'file SymbolId,
    line_count: u32,
    test_search_text: String,
    test_search_truncated: bool,
}

struct FileContainmentInput<'file> {
    file_symbol_id: &'file SymbolId,
    symbols: &'file [NativeSymbolFacts],
    containments: Vec<Containment>,
}

struct FileReferencesInput<'file> {
    identity: &'file FileDocumentIdentity,
    file_symbol_id: &'file SymbolId,
    references: Vec<ExtractedReference>,
    import_bindings: Vec<ExtractedImportBinding>,
    receiver_evidence: Option<Box<ExtractedReceiverEvidence>>,
}

impl ResolutionOutput<'_> {
    fn append_file<Cancel>(
        &mut self,
        file: NativeFileFacts,
        cancelled: &mut Cancel,
    ) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        if cancelled() {
            return Err(StageItemFailure);
        }
        self.budget.charge(file.anticipated_output_bytes())?;
        let NativeFileFacts {
            file,
            line_count,
            symbols,
            containments,
            references,
            call_scope_sites: _,
            javascript_member_calls: _,
            resolution_abstentions: _,
            local_type_scopes: _,
            receiver_evidence,
            numerical_sites,
            import_bindings,
            has_inline_tests: _,
            test_search_text,
            test_search_truncated,
        } = file;
        let file_symbol_id = self
            .index
            .file_symbols
            .get(&file.file_id)
            .ok_or(StageItemFailure)?
            .clone();
        let identity = FileDocumentIdentity {
            file_id: file.file_id.clone(),
            path: try_clone_text(&file.normalized_path)?,
            language: try_clone_text(&file.language)?,
        };
        self.append_file_records(FileRecordInput {
            file: &file,
            identity: &identity,
            file_symbol_id: &file_symbol_id,
            line_count,
            test_search_text,
            test_search_truncated,
        })?;
        self.append_file_containments(
            FileContainmentInput {
                file_symbol_id: &file_symbol_id,
                symbols: &symbols,
                containments,
            },
            cancelled,
        )?;
        self.append_file_references(
            FileReferencesInput {
                identity: &identity,
                file_symbol_id: &file_symbol_id,
                references,
                import_bindings,
                receiver_evidence,
            },
            cancelled,
        )?;
        for site in numerical_sites {
            if cancelled() {
                return Err(StageItemFailure);
            }
            self.facts
                .numerical_sites
                .push(numerical_site_input(&identity.file_id, site));
        }
        for symbol in symbols {
            if cancelled() {
                return Err(StageItemFailure);
            }
            self.append_symbol(&identity, symbol)?;
        }
        self.facts.files.push(file);
        Ok(())
    }

    /// Prepare receiver/import evidence and append references under the file's budget.
    fn append_file_references<Cancel>(
        &mut self,
        input: FileReferencesInput<'_>,
        cancelled: &mut Cancel,
    ) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let FileReferencesInput {
            identity,
            file_symbol_id,
            references,
            import_bindings,
            receiver_evidence,
        } = input;
        let import_binding_index =
            FileImportBindingIndex::new(&import_bindings, self.budget, &identity.language)?;
        let mut import_binding_scratch =
            ImportBindingScratch::new(import_bindings.len(), self.budget)?;
        let current_receivers = generic_resolution::ReceiverSites::new(
            &references,
            (&identity.language, self.index, self.budget),
            cancelled,
        )?;
        let receiver_lookups = receiver_resolution::FileLookups::new(
            receiver_evidence
                .as_deref()
                .map_or(&[], |evidence| evidence.lookups.as_slice()),
            self.budget,
            cancelled,
        )?;
        let context = FileResolutionContext {
            identity,
            file_symbol_id,
            import_bindings: &import_binding_index,
            current_receivers: &current_receivers,
            receiver_lookups: &receiver_lookups,
        };
        for reference in references {
            if cancelled() {
                return Err(StageItemFailure);
            }
            self.append_reference(
                ReferenceAppendRequest {
                    context: &context,
                    reference,
                },
                &mut import_binding_scratch,
                cancelled,
            )?;
        }
        Ok(())
    }

    fn append_file_records(
        &mut self,
        mut input: FileRecordInput<'_>,
    ) -> Result<(), StageItemFailure> {
        self.facts.documents.push(SearchDocumentInput {
            document_id: native_document_id("file", input.identity.file_id.as_str()),
            file_id: Some(input.identity.file_id.clone()),
            symbol_id: Some(input.file_symbol_id.clone()),
            path: try_clone_text(&input.identity.path)?,
            language: try_clone_text(&input.identity.language)?,
            kind: document_kind_for_path(&input.identity.path, DocumentKind::File),
            qualified_name: String::new(),
            code: try_clone_text(&input.identity.path)?,
            natural_text: take(&mut input.test_search_text),
            metadata: json!({
                "byte_size": input.file.byte_size,
                "parse_status": input.file.parse_status.as_str(),
                "test_search_truncated": input.test_search_truncated,
            }),
        });
        self.facts.symbols.push(native_file_symbol_input(&input)?);
        Ok(())
    }

    fn append_file_containments<Cancel>(
        &mut self,
        input: FileContainmentInput<'_>,
        cancelled: &mut Cancel,
    ) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        for symbol in input.symbols {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if !self.index.parents.contains_key(&symbol.input.symbol_id) {
                self.facts.edges.push(EdgeInput {
                    source_symbol_id: input.file_symbol_id.clone(),
                    target_symbol_id: symbol.input.symbol_id.clone(),
                    kind: EdgeKind::Contains,
                    confidence: EXTRACTED_EDGE_CONFIDENCE,
                    provenance: FILE_CONTAINMENT_PROVENANCE.to_owned(),
                    site_count: 1,
                });
            }
        }
        for containment in input.containments {
            if cancelled() {
                return Err(StageItemFailure);
            }
            self.facts.edges.push(EdgeInput {
                source_symbol_id: containment.parent,
                target_symbol_id: containment.child,
                kind: EdgeKind::Contains,
                confidence: EXTRACTED_EDGE_CONFIDENCE,
                provenance: CONTAINMENT_PROVENANCE.to_owned(),
                site_count: 1,
            });
        }
        Ok(())
    }

    fn append_reference<Cancel>(
        &mut self,
        request: ReferenceAppendRequest<'_, '_>,
        import_binding_scratch: &mut ImportBindingScratch,
        cancelled: &mut Cancel,
    ) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let ReferenceAppendRequest { context, reference } = request;
        let resolution = resolve_extracted_reference(
            self.index,
            ExtractedReferenceQuery {
                context,
                reference: &reference,
                import_binding_scratch,
            },
            cancelled,
        )?;
        self.count_resolution(resolution.target.is_some())?;
        let source_symbol_id = reference
            .owner
            .clone()
            .unwrap_or_else(|| context.file_symbol_id.clone());
        if let Some(target) = resolution.target.as_ref()
            && (source_symbol_id != target.symbol_id
                || recursive_call_target(&reference, target)
                || resource_resolution::retain_self_edge(
                    &context.identity.language,
                    reference.kind,
                    target.kind,
                ))
            && (target.provenance != generic_resolution::CURRENT_CLASS_PROVENANCE
                || context.current_receivers.lookup(&reference).is_none())
            && let Some(edge_kind) = reference_edge_kind(
                reference.kind,
                target.kind,
                self.index.types.kind(&source_symbol_id),
            )
        {
            self.facts.edges.push(EdgeInput {
                source_symbol_id,
                target_symbol_id: target.symbol_id.clone(),
                kind: edge_kind,
                confidence: target.confidence,
                provenance: target.provenance.to_owned(),
                site_count: 1,
            });
        }
        self.facts
            .references
            .push(reference_input(ReferenceFactInput {
                file_id: &context.identity.file_id,
                file_symbol_id: context.file_symbol_id,
                reference,
                resolution,
            }));
        Ok(())
    }

    /// Record exactly one resolved or unresolved reference outcome.
    fn count_resolution(&mut self, resolved: bool) -> Result<(), StageItemFailure> {
        let counter = if resolved {
            &mut self.report.resolved
        } else {
            &mut self.report.unresolved
        };
        *counter = counter.checked_add(1).ok_or(StageItemFailure)?;
        Ok(())
    }

    fn append_symbol(
        &mut self,
        identity: &FileDocumentIdentity,
        symbol: NativeSymbolFacts,
    ) -> Result<(), StageItemFailure> {
        let document_id = native_document_id("symbol", symbol.input.symbol_id.as_str());
        let symbol_id = symbol.input.symbol_id.clone();
        let code = symbol_document_code(&symbol)?;
        let partial_clone = symbol.partial_clone.as_ref().map(|evidence| {
            json!({
                "component_size": evidence.component_size,
                "listed_peer_symbol_ids": evidence.listed_peers,
                "maximum_overlap_ppm": evidence.maximum_overlap_ppm,
                "minimum_overlap_ppm": evidence.minimum_overlap_ppm,
                "peer_count": evidence.peer_count,
                "representative_symbol_id": evidence.representative,
            })
        });
        let health = symbol_health_metadata(&symbol.health);
        self.facts.documents.push(SearchDocumentInput {
            document_id,
            file_id: Some(identity.file_id.clone()),
            symbol_id: Some(symbol_id),
            path: try_clone_text(&identity.path)?,
            language: try_clone_text(&identity.language)?,
            kind: if symbol.implementation.test_symbol {
                DocumentKind::Test
            } else {
                document_kind_for_path(&identity.path, DocumentKind::Symbol)
            },
            qualified_name: try_clone_text(&symbol.input.qualified_name)?,
            code,
            natural_text: symbol.docstring.unwrap_or_default(),
            metadata: json!({
                "name": symbol.name,
                "async": symbol.execution.async_symbol,
                "body_search_truncated": symbol.body_search_truncated,
                "clone_shape_digest": symbol.clone_shape_digest.as_str(),
                "near_clone_compatible": symbol.near_clone_compatibility.is_compatible(),
                "duplicate_detection_enabled": symbol.duplicate_detection_enabled,
                "partial_clone": partial_clone,
                "declaration_only": symbol.implementation.declaration_only,
                "test_symbol": symbol.implementation.test_symbol,
                "default_export": symbol.export.default_export,
                "exported": symbol.export.exported,
                "health": health,
                "name": symbol.name,
                "static": symbol.execution.static_member,
                "visibility": symbol.visibility.map(Visibility::as_str),
            }),
        });
        self.facts.symbols.push(symbol.input);
        Ok(())
    }
}

fn symbol_health_metadata(health: &cartograph_extract::SymbolHealthMetrics) -> Value {
    json!(health)
}

fn numerical_site_input(file_id: &FileId, site: ExtractedNumericalSite) -> NumericalSiteInput {
    NumericalSiteInput {
        site_id: site.id,
        file_id: file_id.clone(),
        owner_symbol_id: site.owner,
        start_byte: site.span.start_byte(),
        end_byte: site.span.end_byte(),
        start_line: site.span.start_line(),
        end_line: site.span.end_line(),
        operation: site.operation,
        hazard: site.hazard,
        precision: site.precision,
        expression_digest: site.expression_digest,
        confidence_ppm: site.confidence_ppm,
        provenance: site.provenance,
        evidence_level: "heuristic".to_owned(),
        unknowns: site.unknowns,
    }
}

fn reserve_generation_vectors(
    facts: &mut GenerationFacts,
    extracted: &NativeFactAccumulator,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let files = extracted.files.len();
    let source_symbols = sum_lengths(&extracted.files, |file| file.symbols.len())?;
    let symbols = source_symbols.checked_add(files).ok_or(StageItemFailure)?;
    let containments = sum_lengths(&extracted.files, |file| file.containments.len())?;
    let references = sum_lengths(&extracted.files, |file| file.references.len())?;
    let numerical_sites = sum_lengths(&extracted.files, |file| file.numerical_sites.len())?;
    let edges = containments
        .checked_add(references)
        .and_then(|count| count.checked_add(source_symbols))
        .ok_or(StageItemFailure)?;
    budget.charge(
        usize_to_u64(files)
            .saturating_mul(usize_to_u64(size_of::<FileInput>()))
            .saturating_add(
                usize_to_u64(symbols).saturating_mul(usize_to_u64(size_of::<SymbolInput>())),
            )
            .saturating_add(
                usize_to_u64(edges).saturating_mul(usize_to_u64(size_of::<EdgeInput>())),
            )
            .saturating_add(
                usize_to_u64(references).saturating_mul(usize_to_u64(size_of::<ReferenceInput>())),
            )
            .saturating_add(
                usize_to_u64(numerical_sites)
                    .saturating_mul(usize_to_u64(size_of::<NumericalSiteInput>())),
            )
            .saturating_add(
                usize_to_u64(files.saturating_add(source_symbols))
                    .saturating_mul(usize_to_u64(size_of::<SearchDocumentInput>())),
            ),
    )?;
    facts
        .files
        .try_reserve_exact(files)
        .map_err(|_| StageItemFailure)?;
    facts
        .symbols
        .try_reserve_exact(symbols)
        .map_err(|_| StageItemFailure)?;
    facts
        .edges
        .try_reserve_exact(edges)
        .map_err(|_| StageItemFailure)?;
    facts
        .references
        .try_reserve_exact(references)
        .map_err(|_| StageItemFailure)?;
    facts
        .numerical_sites
        .try_reserve_exact(numerical_sites)
        .map_err(|_| StageItemFailure)?;
    facts
        .documents
        .try_reserve_exact(files.saturating_add(source_symbols))
        .map_err(|_| StageItemFailure)?;
    Ok(())
}

fn sum_lengths(
    files: &[NativeFileFacts],
    length: impl Fn(&NativeFileFacts) -> usize,
) -> Result<usize, StageItemFailure> {
    files.iter().try_fold(0_usize, |total, file| {
        total.checked_add(length(file)).ok_or(StageItemFailure)
    })
}

#[derive(Clone, Copy)]
struct ResolutionCandidateInsertion<'a> {
    key: &'a str,
    symbol: &'a NativeSymbolFacts,
    parent_symbol_id: Option<&'a SymbolId>,
    file_ordinal: u64,
    language: &'a str,
    visibility: Option<Visibility>,
}

#[derive(Clone, Copy)]
struct ModulePathInsertion<'a> {
    key: &'a str,
    file_id: &'a FileId,
}

#[derive(Clone, Copy)]
struct ModuleFileIndexInput<'a> {
    file: &'a FileInput,
    package: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct ModuleStemInput<'a> {
    file: &'a FileInput,
    stem: &'a str,
}

#[derive(Clone, Copy)]
struct DefaultExportInsertion<'a> {
    symbol: &'a NativeSymbolFacts,
    parent_symbol_id: Option<&'a SymbolId>,
}

fn insert_parent(
    parents: &mut ParentMap,
    containment: &Containment,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if let Some(existing) = parents.get(&containment.child) {
        return (existing == &containment.parent).ok_or(StageItemFailure);
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(SymbolId, SymbolId)>()))
            .saturating_add(usize_to_u64(containment.child.as_str().len()))
            .saturating_add(usize_to_u64(containment.parent.as_str().len())),
    )?;
    parents.insert(containment.child.clone(), containment.parent.clone());
    Ok(())
}

fn index_module_path(
    modules: &mut ModulePathIndex,
    input: ModuleFileIndexInput<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    index_resolution_file_context(modules, input, budget)?;
    let file = input.file;
    push_module_path(
        &mut modules.exact,
        ModulePathInsertion {
            key: &file.normalized_path,
            file_id: &file.file_id,
        },
        budget,
    )?;
    let Some(stem) = strip_module_extension(&file.normalized_path, &file.language) else {
        return Ok(());
    };
    index_module_stem(modules, ModuleStemInput { file, stem }, budget)
}

fn index_module_stem(
    modules: &mut ModulePathIndex,
    input: ModuleStemInput<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    push_module_path(
        &mut modules.stem,
        ModulePathInsertion {
            key: input.stem,
            file_id: &input.file.file_id,
        },
        budget,
    )?;
    if let Some(directory) = directory_module_stem(&input.file.language, input.stem) {
        push_module_path(
            &mut modules.directory_index,
            ModulePathInsertion {
                key: directory,
                file_id: &input.file.file_id,
            },
            budget,
        )?;
    }
    Ok(())
}

fn directory_module_stem<'a>(language: &str, stem: &'a str) -> Option<&'a str> {
    let directory = if language == SourceLanguage::Rust.as_str() {
        stem.strip_suffix("/mod")
    } else if javascript_modules::module_language(language) {
        stem.strip_suffix("/index")
    } else if language == SourceLanguage::Python.as_str() {
        stem.strip_suffix("/__init__")
    } else {
        None
    };
    directory.filter(|value| !value.is_empty())
}

fn index_resolution_file_context(
    modules: &mut ModulePathIndex,
    input: ModuleFileIndexInput<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if modules.files.contains_key(&input.file.file_id) {
        return Err(StageItemFailure);
    }
    let directory = input
        .file
        .normalized_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    budget.charge(resolution_file_context_bytes(input, directory))?;
    modules.files.insert(
        input.file.file_id.clone(),
        ResolutionFileContext {
            path: try_clone_text(&input.file.normalized_path)?,
            directory: try_clone_text(directory)?,
            language: try_clone_text(&input.file.language)?,
            package: input.package.map(try_clone_text).transpose()?,
        },
    );
    Ok(())
}

fn resolution_file_context_bytes(input: ModuleFileIndexInput<'_>, directory: &str) -> u64 {
    RESOLUTION_MAP_NODE_ALLOWANCE
        .saturating_add(usize_to_u64(size_of::<(FileId, ResolutionFileContext)>()))
        .saturating_add(usize_to_u64(input.file.file_id.as_str().len()))
        .saturating_add(usize_to_u64(input.file.normalized_path.len()))
        .saturating_add(usize_to_u64(directory.len()))
        .saturating_add(usize_to_u64(input.file.language.len()))
        .saturating_add(input.package.map_or(0, |name| usize_to_u64(name.len())))
}

fn native_package_name(file: &NativeFileFacts) -> Option<&str> {
    if file.file.language != SourceLanguage::Go.as_str() {
        return None;
    }
    file.symbols
        .iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::Module && symbol.input.qualified_name == symbol.name
        })
        .map(|symbol| symbol.name.as_str())
}

fn strip_module_extension<'path>(path: &'path str, language: &str) -> Option<&'path str> {
    const TYPESCRIPT_DECLARATION_EXTENSIONS: [&str; 3] = [".d.mts", ".d.cts", ".d.ts"];
    if language == SourceLanguage::TypeScript.as_str()
        && let Some(stem) = TYPESCRIPT_DECLARATION_EXTENSIONS
            .into_iter()
            .find_map(|extension| strip_suffix_ignore_ascii_case(path, extension))
    {
        return Some(stem);
    }
    let language = SourceLanguage::from_stable_str(language)?;
    language
        .v1_extensions()
        .iter()
        .chain(language.additional_extensions())
        .filter_map(|extension| {
            strip_suffix_ignore_ascii_case(path, extension).map(|stem| (extension.len(), stem))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, stem)| stem)
}

fn strip_any_module_extension(path: &str) -> Option<&str> {
    const TYPESCRIPT_DECLARATION_EXTENSIONS: [&str; 3] = [".d.mts", ".d.cts", ".d.ts"];
    if let Some(stem) = TYPESCRIPT_DECLARATION_EXTENSIONS
        .into_iter()
        .find_map(|extension| strip_suffix_ignore_ascii_case(path, extension))
    {
        return Some(stem);
    }
    SourceLanguage::ALL
        .into_iter()
        .flat_map(|language| {
            language
                .v1_extensions()
                .iter()
                .chain(language.additional_extensions())
        })
        .filter_map(|extension| {
            strip_suffix_ignore_ascii_case(path, extension).map(|stem| (extension.len(), stem))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, stem)| stem)
}

fn push_module_path(
    paths: &mut ModulePathMap,
    insertion: ModulePathInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let ModulePathInsertion { key, file_id } = insertion;
    if !paths.contains_key(key) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(String, Vec<FileId>)>()))
                .saturating_add(usize_to_u64(key.len())),
        )?;
        paths.insert(try_clone_text(key)?, Vec::new());
    }
    let entries = paths.get_mut(key).ok_or(StageItemFailure)?;
    if entries.iter().any(|candidate| candidate == file_id) {
        return Ok(());
    }
    budget.charge(
        usize_to_u64(size_of::<FileId>()).saturating_add(usize_to_u64(file_id.as_str().len())),
    )?;
    entries.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
    entries.push(file_id.clone());
    Ok(())
}

#[derive(Clone, Copy)]
struct CompilationUnitIndexInput<'a> {
    language: &'a str,
    symbol: &'a NativeSymbolFacts,
    top_level: bool,
}

fn index_compilation_unit(
    modules: &mut ModulePathIndex,
    input: CompilationUnitIndexInput<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !input.top_level || !compilation_unit_symbol(input.language, input.symbol) {
        return Ok(());
    }
    let paths = if input.symbol.implementation.declaration_only
        || input.symbol.kind == SymbolKind::Interface
    {
        &mut modules.declared_units
    } else {
        &mut modules.implemented_units
    };
    push_module_path(
        paths,
        ModulePathInsertion {
            key: &input.symbol.name,
            file_id: &input.symbol.input.file_id,
        },
        budget,
    )
}

/// Languages whose imports name a declared compilation unit, not a path.
fn compilation_unit_language(language: &str) -> bool {
    matches!(language, "ada" | "vhdl" | "pascal")
}

/// A top-level declaration that other files import by its declared name:
/// Ada/VHDL packages and entities, and Pascal units (a Pascal program or
/// library is never named by `uses` and is extracted unexported).
fn compilation_unit_symbol(language: &str, symbol: &NativeSymbolFacts) -> bool {
    match language {
        "ada" | "vhdl" => matches!(symbol.kind, SymbolKind::Module | SymbolKind::Interface),
        "pascal" => symbol.kind == SymbolKind::Module && symbol.export.exported,
        _ => false,
    }
}

fn push_candidate(
    candidates: &mut CandidateMap,
    insertion: ResolutionCandidateInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let ResolutionCandidateInsertion {
        key,
        symbol,
        parent_symbol_id,
        file_ordinal,
        language,
        visibility,
    } = insertion;
    if !candidates.contains_key(key) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(
                    size_of::<(String, ResolutionCandidateBucket)>(),
                ))
                .saturating_add(usize_to_u64(key.len())),
        )?;
        candidates.insert(try_clone_text(key)?, ResolutionCandidateBucket::default());
    }
    let bucket = candidates.get_mut(key).ok_or(StageItemFailure)?;
    bucket.native_bridge_member |=
        javascript_member_resolution::native_bridge_symbol(symbol, language);
    let reservation = reserve_candidate_insertion(
        bucket,
        CandidateReservationInput {
            symbol,
            parent_symbol_id,
            file_ordinal,
            language,
            visibility,
        },
        budget,
    )?;
    bucket.candidates.push(ResolutionCandidate {
        file_id: symbol.input.file_id.clone(),
        symbol_id: symbol.input.symbol_id.clone(),
        parent_symbol_id: parent_symbol_id.cloned(),
        qualified_name: try_clone_text(&symbol.input.qualified_name)?,
        signature: try_clone_text(&symbol.input.signature)?,
        kind: symbol.kind,
        visibility,
        implementation: symbol.implementation,
        export: symbol.export,
        top_level: symbol.input.qualified_name == symbol.name,
        augmentation: symbol.augmentation,
        static_member: symbol.execution.static_member,
        declaration_syntax: symbol.declaration_syntax,
        declaration_span: (symbol.input.start_byte, symbol.input.end_byte),
    });
    append_candidate_project_index(bucket, language, reservation)?;
    append_candidate_file_range(bucket, file_ordinal, reservation)
}

#[derive(Clone, Copy)]
struct CandidateReservation {
    candidate_index: usize,
    new_file_range: bool,
    globally_visible: bool,
}

#[derive(Clone, Copy)]
struct CandidateReservationInput<'a> {
    symbol: &'a NativeSymbolFacts,
    parent_symbol_id: Option<&'a SymbolId>,
    file_ordinal: u64,
    language: &'a str,
    visibility: Option<Visibility>,
}

fn reserve_candidate_insertion(
    bucket: &mut ResolutionCandidateBucket,
    input: CandidateReservationInput<'_>,
    budget: &mut ResolveBudget,
) -> Result<CandidateReservation, StageItemFailure> {
    let CandidateReservationInput {
        symbol,
        parent_symbol_id: _,
        file_ordinal,
        language,
        visibility,
    } = input;
    let candidate_index = bucket.candidates.len();
    let new_file_range = bucket
        .by_file
        .last()
        .is_none_or(|range| range.file_ordinal != file_ordinal);
    let globally_visible = symbol.export.exported || visibility == Some(Visibility::Public);
    let new_language_bucket =
        !globally_visible && !bucket.non_visible_by_language.contains_key(language);
    let retained = candidate_reservation_bytes(input, new_file_range, new_language_bucket);
    budget.charge(retained)?;
    bucket
        .candidates
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    if new_file_range {
        bucket
            .by_file
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
    }
    reserve_candidate_project_index(
        bucket,
        CandidateLanguageVisibility {
            language,
            globally_visible,
            new_language_bucket,
        },
    )?;
    Ok(CandidateReservation {
        candidate_index,
        new_file_range,
        globally_visible,
    })
}

fn candidate_reservation_bytes(
    input: CandidateReservationInput<'_>,
    new_file_range: bool,
    new_language_bucket: bool,
) -> u64 {
    let CandidateReservationInput {
        symbol,
        parent_symbol_id,
        file_ordinal: _,
        language,
        visibility: _,
    } = input;
    let file_range_bytes = if new_file_range {
        usize_to_u64(size_of::<ResolutionCandidateRange>())
    } else {
        0
    };
    let language_bucket_bytes = new_language_bucket.then(|| {
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(String, Vec<usize>)>()))
            .saturating_add(usize_to_u64(language.len()))
    });
    usize_to_u64(size_of::<ResolutionCandidate>())
        .saturating_add(usize_to_u64(symbol.input.file_id.as_str().len()))
        .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len()))
        .saturating_add(usize_to_u64(symbol.input.qualified_name.len()))
        .saturating_add(usize_to_u64(symbol.input.signature.len()))
        .saturating_add(parent_symbol_id.map_or(0, |parent| usize_to_u64(parent.as_str().len())))
        .saturating_add(file_range_bytes)
        .saturating_add(usize_to_u64(size_of::<usize>()))
        .saturating_add(language_bucket_bytes.unwrap_or_default())
}

fn reserve_candidate_project_index(
    bucket: &mut ResolutionCandidateBucket,
    reservation: CandidateLanguageVisibility<'_>,
) -> Result<(), StageItemFailure> {
    let CandidateLanguageVisibility {
        language,
        globally_visible,
        new_language_bucket,
    } = reservation;
    if globally_visible {
        return bucket
            .globally_visible
            .try_reserve_exact(1)
            .map_err(|_| StageItemFailure);
    }
    if new_language_bucket {
        bucket
            .non_visible_by_language
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        bucket
            .non_visible_by_language
            .insert(try_clone_text(language)?, Vec::new());
    }
    bucket
        .non_visible_by_language
        .get_mut(language)
        .ok_or(StageItemFailure)?
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)
}

fn append_candidate_project_index(
    bucket: &mut ResolutionCandidateBucket,
    language: &str,
    reservation: CandidateReservation,
) -> Result<(), StageItemFailure> {
    if reservation.globally_visible {
        bucket.globally_visible.push(reservation.candidate_index);
    } else {
        bucket
            .non_visible_by_language
            .get_mut(language)
            .ok_or(StageItemFailure)?
            .push(reservation.candidate_index);
    }
    Ok(())
}

fn append_candidate_file_range(
    bucket: &mut ResolutionCandidateBucket,
    file_ordinal: u64,
    reservation: CandidateReservation,
) -> Result<(), StageItemFailure> {
    let next_index = reservation
        .candidate_index
        .checked_add(1)
        .ok_or(StageItemFailure)?;
    if let Some(range) = bucket.by_file.last_mut() {
        if range.file_ordinal == file_ordinal {
            if range.end != reservation.candidate_index {
                return Err(StageItemFailure);
            }
            range.end = next_index;
        } else {
            if range.file_ordinal >= file_ordinal {
                return Err(StageItemFailure);
            }
            bucket.by_file.push(ResolutionCandidateRange {
                file_ordinal,
                start: reservation.candidate_index,
                end: next_index,
            });
        }
    } else {
        if !reservation.new_file_range {
            return Err(StageItemFailure);
        }
        bucket.by_file.push(ResolutionCandidateRange {
            file_ordinal,
            start: reservation.candidate_index,
            end: next_index,
        });
    }
    Ok(())
}

fn push_default_export(
    exports: &mut DefaultExportMap,
    insertion: DefaultExportInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let DefaultExportInsertion {
        symbol,
        parent_symbol_id,
    } = insertion;
    if !exports.contains_key(&symbol.input.file_id) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(FileId, Vec<ResolutionCandidate>)>()))
                .saturating_add(usize_to_u64(symbol.input.file_id.as_str().len())),
        )?;
        exports.insert(symbol.input.file_id.clone(), Vec::new());
    }
    let entries = exports
        .get_mut(&symbol.input.file_id)
        .ok_or(StageItemFailure)?;
    budget.charge(
        usize_to_u64(size_of::<ResolutionCandidate>())
            .saturating_add(usize_to_u64(symbol.input.file_id.as_str().len()))
            .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len()))
            .saturating_add(usize_to_u64(symbol.input.qualified_name.len()))
            .saturating_add(usize_to_u64(symbol.input.signature.len()))
            .saturating_add(
                parent_symbol_id.map_or(0, |parent| usize_to_u64(parent.as_str().len())),
            ),
    )?;
    entries.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
    entries.push(ResolutionCandidate {
        file_id: symbol.input.file_id.clone(),
        symbol_id: symbol.input.symbol_id.clone(),
        parent_symbol_id: parent_symbol_id.cloned(),
        qualified_name: try_clone_text(&symbol.input.qualified_name)?,
        signature: try_clone_text(&symbol.input.signature)?,
        kind: symbol.kind,
        visibility: symbol.visibility,
        implementation: symbol.implementation,
        export: symbol.export,
        top_level: symbol.input.qualified_name == symbol.name,
        augmentation: symbol.augmentation,
        static_member: symbol.execution.static_member,
        declaration_syntax: symbol.declaration_syntax,
        declaration_span: (symbol.input.start_byte, symbol.input.end_byte),
    });
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReferenceDispatch {
    Static,
    Dynamic,
    RustSelf,
}

struct ResolutionRequest<'a> {
    file_id: &'a FileId,
    file_path: &'a str,
    language: &'a str,
    import_bindings: ImportBindingSelection<'a>,
    owner: Option<&'a SymbolId>,
    name: &'a str,
    dispatch: ReferenceDispatch,
    kind: ReferenceKind,
    span: SourceSpan,
}

#[derive(Clone, Copy)]
struct ProjectCandidateInput<'a> {
    modules: &'a ModulePathIndex,
    source: &'a ResolutionFileContext,
    source_file_id: &'a FileId,
    reference_name: &'a str,
    dynamic_dispatch: bool,
    rust_local_import: bool,
    candidate: &'a ResolutionCandidate,
}

struct AppleBridgeQuery<'context, 'request, Cancel> {
    index: &'context ResolutionIndex,
    source: &'context ResolutionFileContext,
    request: &'context ResolutionRequest<'request>,
    cancelled: &'context mut Cancel,
}

struct AppleBridgeSelection<'context, 'request, 'candidate, Cancel> {
    index: &'context ResolutionIndex,
    source: &'context ResolutionFileContext,
    request: &'context ResolutionRequest<'request>,
    candidates: &'candidate [ResolutionCandidate],
    target_language: &'context str,
    cancelled: &'context mut Cancel,
}

struct FrameworkSelection<'context, 'request, Candidates, Cancel> {
    index: &'context ResolutionIndex,
    source: &'context ResolutionFileContext,
    request: &'context ResolutionRequest<'request>,
    candidates: Candidates,
    cancelled: &'context mut Cancel,
}

#[derive(Clone, Copy)]
struct FrameworkCandidateInput<'context> {
    modules: &'context ModulePathIndex,
    source: &'context ResolutionFileContext,
    source_file_id: &'context FileId,
    candidate: &'context ResolutionCandidate,
}

#[derive(Clone, Copy)]
struct PhpRouteCandidateInput<'context> {
    source: &'context ResolutionFileContext,
    target: &'context ResolutionFileContext,
    source_file_id: &'context FileId,
    candidate: &'context ResolutionCandidate,
}

struct ReferenceFactInput<'context> {
    file_id: &'context FileId,
    file_symbol_id: &'context SymbolId,
    reference: ExtractedReference,
    resolution: ReferenceResolution,
}

#[derive(Clone, Copy)]
enum ImportReferenceSite {
    Declaration,
    Usage,
}

#[derive(Clone, Copy)]
struct ImportResolutionRequest<'a, 'b> {
    reference: &'a ResolutionRequest<'b>,
    site: ImportReferenceSite,
}

struct ImportCandidateFilter<'index, 'request> {
    index: &'index ResolutionIndex,
    reference: &'index ResolutionRequest<'request>,
    imported_name: &'index str,
    module_file_id: &'index FileId,
    javascript_value_usage: bool,
}

impl ImportCandidateFilter<'_, '_> {
    fn matches(&self, candidate: &ResolutionCandidate) -> bool {
        if &candidate.file_id != self.module_file_id || !candidate.export.exported {
            return false;
        }
        if !rust_use_bindings::visible(self.index, self.reference, candidate) {
            return false;
        }
        if !reference_kind_candidate(self.reference.kind, candidate) {
            return false;
        }
        if self.javascript_value_usage && !javascript_runtime_import_candidate(candidate) {
            return false;
        }
        if self.reference.language == SourceLanguage::Rust.as_str()
            && !rust_module_candidate_visible(RustCandidateVisibility {
                index: self.index,
                candidate,
                target_name: self.imported_name,
                source_path: self.reference.file_path,
            })
        {
            return false;
        }
        true
    }
}

#[derive(Clone, Copy)]
struct ModuleResolutionRequest<'a> {
    importing_path: &'a str,
    specifier: &'a str,
    importing_language: &'a str,
}

enum TypeScriptAliasModuleResolution<'a> {
    NotMatched,
    Resolved(&'a FileId, bool),
    Unresolved,
    Ambiguous,
}

#[derive(Clone, Copy)]
struct RustQualifiedModuleQuery<'context, 'request> {
    index: &'context ResolutionIndex,
    request: &'context ResolutionRequest<'request>,
    module_specifier: &'context str,
    target_name: &'context str,
    allow_unique_project_reexport: bool,
}

#[derive(Clone, Copy)]
struct RustCandidateVisibility<'context> {
    index: &'context ResolutionIndex,
    candidate: &'context ResolutionCandidate,
    target_name: &'context str,
    source_path: &'context str,
}

#[derive(Clone, Copy)]
struct IncludeImplementationQuery<'context, 'request> {
    index: &'context ResolutionIndex,
    request: &'context ResolutionRequest<'request>,
    declaration: &'context ResolutionCandidate,
}

#[derive(Clone, Copy)]
struct RustNamespaceImportQuery<'context, 'request> {
    index: &'context ResolutionIndex,
    reference: &'context ResolutionRequest<'request>,
    binding: &'context ExtractedImportBinding,
}

#[derive(Clone, Copy)]
struct ImportCandidatesQuery<'context> {
    index: &'context ResolutionIndex,
    binding: &'context ExtractedImportBinding,
    imported_name: &'context str,
    module_file_id: &'context FileId,
}

fn resolve_embedded_sql<Cancel>(
    index: &ResolutionIndex,
    lookup: EmbeddedSqlLookup<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let candidates = index.candidates.get(lookup.table).map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    let candidate = select_candidate(
        candidates,
        |candidate| {
            candidate.kind == SymbolKind::Table
                && !candidate.augmentation
                && index
                    .modules
                    .files
                    .get(&candidate.file_id)
                    .is_some_and(|file| file.language == SourceLanguage::Sql.as_str())
                && (candidate.qualified_name.eq_ignore_ascii_case(lookup.table)
                    || (!lookup.table.contains('.')
                        && candidate
                            .qualified_name
                            .rsplit('.')
                            .next()
                            .is_some_and(|name| name.eq_ignore_ascii_case(lookup.table))))
        },
        cancelled,
    )?;
    let Some(candidate) = candidate else {
        return Ok(ReferenceResolution::unresolved(
            lookup.operation.unresolved_provenance(),
        ));
    };
    Ok(ReferenceResolution::resolved(ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: EMBEDDED_SQL_CONFIDENCE,
        provenance: lookup.operation.resolved_provenance(),
    }))
}

/// A self edge is a proven recursive call only through the same-file lexical
/// scope walk; a project-wide name match on the owner (Objective-C
/// `[super m]` inside `m`) is not recursion.
fn recursive_call_target(reference: &ExtractedReference, target: &ResolvedTarget) -> bool {
    reference.kind == ReferenceKind::Calls
        && matches!(
            target.provenance,
            EXACT_SAME_FILE_PROVENANCE | EXACT_LEXICAL_PROVENANCE
        )
}

fn resolve_declaration_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.import_bindings.fallback_blocked {
        return Ok(None);
    }
    if let Some(resolution) = resolve_module_declaration_reference(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if request.owner.is_none()
        && request.kind == ReferenceKind::References
        && let Some(resolution) = import_reference_resolution(resolve_import(
            index,
            ImportResolutionRequest {
                reference: request,
                site: ImportReferenceSite::Declaration,
            },
            cancelled,
        )?)
    {
        return Ok(Some(resolution));
    }
    if matches!(request.kind, ReferenceKind::TypeOf | ReferenceKind::Returns)
        && let Some(binding) = request
            .import_bindings
            .iter()
            .find(|binding| binding.span == request.span)
    {
        // An inline import type (`import('./m').Name`) names its module at the
        // reference site itself. It resolves only through that site's binding,
        // never through a same-named local or binding elsewhere in the file.
        let site = resolve_import(
            index,
            ImportResolutionRequest {
                reference: request,
                site: ImportReferenceSite::Declaration,
            },
            cancelled,
        )?;
        return Ok(Some(match site {
            ImportResolution::Resolved(target) => ReferenceResolution::resolved(target),
            ImportResolution::Unresolved | ImportResolution::NotBound
                if import_binding_is_project_local(index, binding, request) =>
            {
                ReferenceResolution::unresolved(UNRESOLVED_IMPORT_PROVENANCE)
            }
            ImportResolution::Unresolved | ImportResolution::NotBound => {
                ReferenceResolution::unresolved(EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE)
            }
        }));
    }
    if request.kind == ReferenceKind::Exports
        && let Some(resolution) = import_reference_resolution(resolve_import(
            index,
            ImportResolutionRequest {
                reference: request,
                site: ImportReferenceSite::Usage,
            },
            cancelled,
        )?)
    {
        return Ok(Some(resolution));
    }
    Ok(None)
}

/// Resolve declarations that name a file or module before looking up imported symbols.
fn resolve_module_declaration_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(target) = repr_file_imports::resolve(index, request, cancelled)? {
        return Ok(Some(ReferenceResolution::resolved(target)));
    }
    if request.language == SourceLanguage::Liquid.as_str()
        && let Some(target) = file_path_resolution::resolve_reference(index, request, cancelled)?
    {
        return Ok(Some(ReferenceResolution::resolved(target)));
    }
    if request.kind == ReferenceKind::Imports
        && request.owner.is_none()
        && let Some(resolution) =
            import_reference_resolution(resolve_include_file_reference(index, request, cancelled)?)
    {
        return Ok(Some(resolution));
    }
    if request.kind == ReferenceKind::Imports
        && request.owner.is_none()
        && (script_modules::binds_loads_exactly(request.language)
            || request.language == SourceLanguage::Python.as_str())
        && let Some(resolution) = import_reference_resolution(resolve_module_import_file_reference(
            index, request, cancelled,
        )?)
    {
        return Ok(Some(resolution));
    }
    Ok(None)
}

/// Resolve a Terraform address (`var.x`, `local.x`, `module.x`, `T.N`,
/// `data.T.N`) terminally: a Terraform module is one directory, and an
/// address names exactly the HCL declaration whose qualified name it equals.
/// Display names (`T.N` is also a data source's name) and other languages
/// never satisfy it; duplicates and self-references stay unresolved.
fn resolve_terraform_address<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != SourceLanguage::Hcl.as_str() || request.kind != ReferenceKind::References
    {
        return Ok(None);
    }
    let source = project_source_context(index, request)?;
    let candidates = index.candidates.get(request.name).map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    let mut declaration = None;
    let mut declarations = 0_usize;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let in_module = index
            .modules
            .files
            .get(&candidate.file_id)
            .is_some_and(|target| {
                target.language == source.language && target.directory == source.directory
            });
        if in_module && candidate.qualified_name == request.name {
            declarations = declarations.saturating_add(1);
            declaration = Some(candidate);
        }
    }
    let target = declaration
        .filter(|candidate| declarations == 1 && request.owner != Some(&candidate.symbol_id));
    Ok(Some(target.map_or(
        ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE),
        |candidate| {
            let (confidence, provenance) = if &candidate.file_id == request.file_id {
                (EXACT_SAME_FILE_CONFIDENCE, EXACT_SAME_FILE_PROVENANCE)
            } else {
                (EXACT_PROJECT_CONFIDENCE, EXACT_PROJECT_PROVENANCE)
            };
            ReferenceResolution::resolved(ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence,
                provenance,
            })
        },
    )))
}

/// A resolution request plus the reference's source-visible name.
#[derive(Clone, Copy)]
struct SourceNameFallbackQuery<'request, 'name> {
    request: &'request ResolutionRequest<'name>,
    source_name: &'name str,
}

/// Resolve by the reference's exact lookup name and, only for `MyBatis` XML
/// references that carry a package-qualified lookup name
/// (`com.example::OrderMapper::findOrder::orderId`), retry once with the exact
/// source-visible name (`OrderMapper::findOrder::orderId`). The retry can only
/// match an unpackaged JVM declaration (a packaged one never has that exact
/// name), keeps the pre-existing simple-namespace binding, and is recorded with
/// framework-convention confidence and provenance rather than as exact.
fn resolve_reference_or_source_name<Cancel>(
    index: &ResolutionIndex,
    query: SourceNameFallbackQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let SourceNameFallbackQuery {
        request,
        source_name,
    } = query;
    let resolution = resolve_reference(index, request, cancelled)?;
    let package_qualified = request
        .name
        .strip_suffix(source_name)
        .is_some_and(|package| package.len() > "::".len() && package.ends_with("::"));
    // Any declaration carrying the exact package-qualified name (even an
    // ambiguous set) is authoritative; the compatibility lookup only runs when
    // no packaged declaration exists at all.
    if resolution.target.is_some()
        || request.language != SourceLanguage::Xml.as_str()
        || !package_qualified
        || index.candidates.contains_key(request.name)
    {
        return Ok(resolution);
    }
    let mut fallback = resolve_reference(
        index,
        &ResolutionRequest {
            name: source_name,
            ..*request
        },
        cancelled,
    )?;
    let Some(target) = fallback.target.as_mut() else {
        return Ok(resolution);
    };
    // The package named by the mapper namespace is absent from the target, so
    // this binding is the legacy simple-namespace convention, not exact identity.
    target.confidence = FRAMEWORK_CONVENTION_CONFIDENCE;
    target.provenance = FRAMEWORK_CONVENTION_PROVENANCE;
    Ok(fallback)
}

fn resolve_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(target) = framework_imports::resolve(index, request, cancelled)? {
        return Ok(ReferenceResolution::resolved(target));
    }
    let mut resolution = match rust_path_guards::resolve(index, request, cancelled)? {
        Some(resolution) => Ok(resolution),
        None => resolve_reference_body(index, request, cancelled),
    }?;
    if resolution.target.is_none()
        && !index.candidates.contains_key(request.name)
        && let Some(provenance) = framework_provided::provenance(request, cancelled)?
    {
        resolution = ReferenceResolution::unresolved(provenance);
    }
    Ok(resolution)
}

fn resolve_reference_body<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.dispatch == ReferenceDispatch::RustSelf && !request.name.contains("::") {
        return Ok(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        ));
    }
    if request.kind == ReferenceKind::DefUse {
        return resolve_def_use(index, request, cancelled);
    }
    if let Some(resolution) = framework_resolution::resolve(index, request, cancelled)? {
        return Ok(resolution);
    }
    if let Some(resolution) = resolve_terraform_address(index, request, cancelled)? {
        return Ok(resolution);
    }
    if let Some(resolution) = qualified_member_resolution::resolve(index, request, cancelled)? {
        return Ok(resolution);
    }
    if let Some(resolution) = jvm_resolution::resolve_reference(index, request, cancelled)? {
        return Ok(resolution);
    }
    if let Some(resolution) = resolve_declaration_reference(index, request, cancelled)? {
        return Ok(resolution);
    }
    reference_dispatch::resolve_remaining(index, request, cancelled)
}

/// Try module and import bindings before the permitted project-wide fallbacks.
fn resolve_import_or_project_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(resolution) = import_reference_resolution(resolve_module_import_file_reference(
        index, request, cancelled,
    )?) {
        return Ok(resolution);
    }
    if let Some(resolution) =
        import_reference_resolution(resolve_include_bound_symbol(index, request, cancelled)?)
    {
        return Ok(resolution);
    }
    if let Some(resolution) = import_reference_resolution(resolve_import(
        index,
        ImportResolutionRequest {
            reference: request,
            site: ImportReferenceSite::Usage,
        },
        cancelled,
    )?) {
        return Ok(resolution);
    }
    if project_fallback_allowed(index, request)
        && let Some(target) = resolve_project(index, request, cancelled)?
    {
        return Ok(ReferenceResolution::resolved(target));
    }
    if project_fallback_allowed(index, request)
        && let Some(target) = generic_resolution::resolve_name_fallback(index, request, cancelled)?
    {
        return Ok(ReferenceResolution::resolved(target));
    }
    Ok(ReferenceResolution::unresolved(
        unresolved_reference_provenance(index, request, cancelled)?,
    ))
}

fn unresolved_reference_provenance<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<&'static str, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(provenance) = fixed_unresolved_provenance(request) {
        return Ok(provenance);
    }
    let import_scope = reference_import_scope(index, request, cancelled)?;
    if let Some(provenance) = import_scope_unresolved_provenance(import_scope, request) {
        return Ok(provenance);
    }
    if rust_explicit_external_path(request) {
        return Ok(RUST_EXTERNAL_UNRESOLVED_PROVENANCE);
    }
    if has_project_candidate(index, request) {
        return Ok(UNRESOLVED_PROVENANCE);
    }
    Ok(language_unresolved_provenance(request).unwrap_or(UNRESOLVED_PROVENANCE))
}

fn fixed_unresolved_provenance(request: &ResolutionRequest<'_>) -> Option<&'static str> {
    if request.dispatch != ReferenceDispatch::Static {
        Some(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)
    } else if request.kind == ReferenceKind::FieldAccess {
        Some(MEMBER_ACCESS_UNRESOLVED_PROVENANCE)
    } else if request.language == SourceLanguage::Rust.as_str() && rust_intrinsic_reference(request)
    {
        Some(RUST_INTRINSIC_UNRESOLVED_PROVENANCE)
    } else if pascal_resolution::runtime_reference(request.language, request.name) {
        Some(EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE)
    } else {
        None
    }
}

fn import_scope_unresolved_provenance(
    scope: ImportScope,
    request: &ResolutionRequest<'_>,
) -> Option<&'static str> {
    match scope {
        ImportScope::Local => Some(UNRESOLVED_IMPORT_PROVENANCE),
        ImportScope::NonLocal if request.language == SourceLanguage::Rust.as_str() => {
            Some(RUST_EXTERNAL_UNRESOLVED_PROVENANCE)
        }
        ImportScope::NonLocal => Some(EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE),
        ImportScope::None | ImportScope::Ambiguous => None,
    }
}

fn language_unresolved_provenance(request: &ResolutionRequest<'_>) -> Option<&'static str> {
    if let Some(provenance) = javascript_unresolved_provenance(request) {
        return Some(provenance);
    }
    if let Some(provenance) = python_unresolved_provenance(request) {
        return Some(provenance);
    }
    if shell_command_language(request.language) && request.kind == ReferenceKind::Calls {
        return Some(SHELL_COMMAND_UNRESOLVED_PROVENANCE);
    }
    if request.language == SourceLanguage::Toml.as_str()
        && request.kind == ReferenceKind::References
    {
        return Some(MANIFEST_REFERENCE_UNRESOLVED_PROVENANCE);
    }
    None
}

fn javascript_unresolved_provenance(request: &ResolutionRequest<'_>) -> Option<&'static str> {
    if !javascript_family_name(request.language) {
        return None;
    }
    if javascript_intrinsic_reference(request) {
        return Some(JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE);
    }
    (request.kind == ReferenceKind::Calls && javascript_receiver_reference(request.name))
        .then_some(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)
}

fn python_unresolved_provenance(request: &ResolutionRequest<'_>) -> Option<&'static str> {
    if request.language != SourceLanguage::Python.as_str() {
        return None;
    }
    if python_intrinsic_reference(request) {
        return Some(PYTHON_INTRINSIC_UNRESOLVED_PROVENANCE);
    }
    (request.kind == ReferenceKind::Calls && python_receiver_reference(request.name))
        .then_some(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE)
}

fn python_intrinsic_reference(request: &ResolutionRequest<'_>) -> bool {
    !request.name.contains('.')
        && (PythonExceptions.contains(request.name) || PythonValues.contains(request.name))
}

fn python_receiver_reference(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(receiver, member)| {
        !receiver.trim().is_empty()
            && !member.trim().is_empty()
            && member
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    })
}

fn has_project_candidate(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    index
        .candidates
        .get(request.name)
        .is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                reference_kind_candidate(request.kind, candidate)
                    && index
                        .modules
                        .files
                        .get(&candidate.file_id)
                        .is_some_and(|file| {
                            resolution_languages_compatible(request.language, &file.language)
                        })
            })
        })
}

fn rust_intrinsic_reference(request: &ResolutionRequest<'_>) -> bool {
    let root = request.name.split("::").next().unwrap_or(request.name);
    if matches!(root, "std" | "core" | "alloc") {
        return true;
    }
    let intrinsic_type = matches!(
        root,
        "Self"
            | "Cow"
            | "Fn"
            | "FnMut"
            | "FnOnce"
            | "Result"
            | "Option"
            | "String"
            | "str"
            | "Vec"
            | "Box"
            | "bool"
            | "char"
            | "f32"
            | "f64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
    );
    if intrinsic_type {
        return true;
    }
    request.kind == ReferenceKind::Calls
        && matches!(request.name, "Ok" | "Err" | "Some" | "None" | "drop")
}

fn rust_explicit_external_path(request: &ResolutionRequest<'_>) -> bool {
    if request.language != SourceLanguage::Rust.as_str() {
        return false;
    }
    let Some((root, _)) = request.name.split_once("::") else {
        return false;
    };
    !matches!(root, "crate" | "self" | "super" | "Self")
        && root.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
}

fn javascript_intrinsic_reference(request: &ResolutionRequest<'_>) -> bool {
    let root = request.name.split('.').next().unwrap_or(request.name);
    matches!(
        root,
        "Array"
            | "ArrayBuffer"
            | "BigInt"
            | "Boolean"
            | "Buffer"
            | "Date"
            | "Error"
            | "EvalError"
            | "Intl"
            | "JSON"
            | "Map"
            | "Math"
            | "Number"
            | "Object"
            | "Parameters"
            | "Promise"
            | "RangeError"
            | "ReferenceError"
            | "Reflect"
            | "RegExp"
            | "Set"
            | "String"
            | "Symbol"
            | "SyntaxError"
            | "TextDecoder"
            | "TextEncoder"
            | "TypeError"
            | "URIError"
            | "URL"
            | "URLSearchParams"
            | "WeakMap"
            | "WeakSet"
            | "clearInterval"
            | "clearTimeout"
            | "console"
            | "fetch"
            | "globalThis"
            | "parseFloat"
            | "parseInt"
            | "process"
            | "queueMicrotask"
            | "setInterval"
            | "setTimeout"
            | "structuredClone"
    )
}

fn javascript_receiver_reference(name: &str) -> bool {
    name.split_once('.').is_some_and(|(receiver, member)| {
        !receiver.is_empty()
            && !member.is_empty()
            && (receiver == "this"
                || receiver == "super"
                || receiver
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase))
    })
}

fn shell_command_language(language: &str) -> bool {
    matches!(language, "bash" | "fish" | "powershell" | "zsh")
}

fn import_reference_resolution(resolution: ImportResolution) -> Option<ReferenceResolution> {
    match resolution {
        ImportResolution::NotBound => None,
        ImportResolution::Resolved(target) => Some(ReferenceResolution::resolved(target)),
        ImportResolution::Unresolved => Some(ReferenceResolution::unresolved(
            UNRESOLVED_IMPORT_PROVENANCE,
        )),
    }
}

fn project_fallback_allowed(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    if request.dispatch == ReferenceDispatch::RustSelf
        || pascal_resolution::runtime_reference(request.language, request.name)
    {
        return false;
    }
    let runtime_require = javascript_family_name(request.language)
        && request.kind == ReferenceKind::Calls
        && request.name == "require";
    let external_binding = request.import_bindings.iter().any(|binding| {
        binding_matches_reference_name(binding, request.name)
            && !import_binding_is_project_local(index, binding, request)
            && !wildcard_import_yields_to_project(request.language, binding)
    });
    !runtime_require && !external_binding
}

/// Swift looks a name up in the importing module before any imported module,
/// and Objective-C classes and selectors share one global runtime namespace, so
/// in both a wildcard import of an external module (`import Foundation`,
/// `#import <UIKit/UIKit.h>`, written in nearly every file) cannot hide the
/// project's own declaration of a name and never vetoes project resolution.
fn wildcard_import_yields_to_project(language: &str, binding: &ExtractedImportBinding) -> bool {
    binding.local_name == "*"
        && (language == SourceLanguage::Swift.as_str()
            || language == SourceLanguage::ObjectiveC.as_str())
}

fn resolve_lexical<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let exact_name = if request.dispatch == ReferenceDispatch::RustSelf {
        request.name.strip_prefix("self::").unwrap_or(request.name)
    } else {
        request.name
    };
    let candidates = resolution_candidates_for_file(index, exact_name, request.file_id);
    if let Some(candidate) = select_candidate(
        candidates,
        |candidate| {
            &candidate.file_id == request.file_id
                && candidate.qualified_name == exact_name
                && (request.dispatch == ReferenceDispatch::RustSelf
                    || request.owner != Some(&candidate.symbol_id))
                && reference_kind_candidate(request.kind, candidate)
        },
        cancelled,
    )? {
        return Ok(Some(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: EXACT_SAME_FILE_CONFIDENCE,
            provenance: EXACT_SAME_FILE_PROVENANCE,
        }));
    }
    if pascal_resolution::exact_scope_only(request.language, request.dispatch) {
        return Ok(None);
    }
    resolve_lexical_scope(
        index,
        LexicalScopeQuery {
            request,
            candidates,
        },
        cancelled,
    )
}

/// Resolve a JavaScript-family read the extractor marked as bound by a nested
/// lexical declaration whose qualified name a namesake shares: only the scope
/// walk from the read's owner binds it, never the exact same-file
/// qualified-name pass, an import, or a project name.
fn resolve_lexical_scope_reference<Cancel>(
    index: &ResolutionIndex,
    query: LexicalScopeReference<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let LexicalScopeReference {
        context,
        reference,
        name,
    } = query;
    let request = ResolutionRequest {
        file_id: &context.identity.file_id,
        file_path: &context.identity.path,
        language: &context.identity.language,
        import_bindings: ImportBindingSelection::empty(),
        owner: reference.owner.as_ref(),
        name,
        dispatch: ReferenceDispatch::Static,
        kind: reference.kind,
        span: reference.span,
    };
    let candidates = resolution_candidates_for_file(index, name, request.file_id);
    Ok(
        match resolve_lexical_scope(
            index,
            LexicalScopeQuery {
                request: &request,
                candidates,
            },
            cancelled,
        )? {
            Some(target) => ReferenceResolution::resolved(target),
            None => ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE),
        },
    )
}

/// A lexical-scope-marked reference and the name its scope walk binds.
#[derive(Clone, Copy)]
struct LexicalScopeReference<'a, 'b> {
    context: &'a FileResolutionContext<'b>,
    reference: &'a ExtractedReference,
    name: &'a str,
}

/// A lexical scope walk's request and the same-file candidates it binds from.
#[derive(Clone, Copy)]
struct LexicalScopeQuery<'request, 'a> {
    request: &'request ResolutionRequest<'a>,
    candidates: &'request [ResolutionCandidate],
}

/// Walk the reference owner's enclosing scopes outward to the file's top
/// level and bind the first scope's declaration of the name.
fn resolve_lexical_scope<Cancel>(
    index: &ResolutionIndex,
    query: LexicalScopeQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let LexicalScopeQuery {
        request,
        candidates,
    } = query;
    let mut scope = request.owner;
    // There can be one more lexical scope than parent-map entries: an
    // uncontained owner must still advance once to the file's top-level scope.
    for _ in 0..=index.parents.len().saturating_add(1) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(candidate) = select_candidate(
            candidates,
            |candidate| {
                lexical_candidate_matches(index, request, candidate)
                    && candidate.parent_symbol_id.as_ref() == scope
            },
            cancelled,
        )? {
            let (confidence, provenance) = if candidate.parent_symbol_id.is_some() {
                (EXACT_LEXICAL_CONFIDENCE, EXACT_LEXICAL_PROVENANCE)
            } else {
                (EXACT_SAME_FILE_CONFIDENCE, EXACT_SAME_FILE_PROVENANCE)
            };
            return Ok(Some(ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence,
                provenance,
            }));
        }
        let Some(symbol_id) = scope else {
            return Ok(None);
        };
        scope = index.parents.get(symbol_id);
    }
    Ok(None)
}

/// Check same-file lexical eligibility, including the proof needed for recursion.
fn lexical_candidate_matches(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    (is_lexical_candidate(request.kind, request.name, candidate)
        || swift_implicit_member_call(request, candidate))
        && &candidate.file_id == request.file_id
        && (request.owner != Some(&candidate.symbol_id)
            || generic_resolution::recursive_owner_call(index, request, candidate))
}

/// Resolve a def-use site to the local it reads: the unique same-file binding
/// declared directly in the owning callable's scope.
///
/// A local is never bound through outer scopes, imports, or project names; a
/// name declared more than once in the callable stays unresolved with its
/// sites retained.
fn resolve_def_use<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(owner) = request.owner else {
        return Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE));
    };
    let candidates = resolution_candidates_for_file(index, request.name, request.file_id);
    let local = select_candidate(
        candidates,
        |candidate| {
            &candidate.file_id == request.file_id
                && candidate.parent_symbol_id.as_ref() == Some(owner)
                && matches!(
                    candidate.kind,
                    SymbolKind::Variable
                        | SymbolKind::Constant
                        | SymbolKind::Function
                        | SymbolKind::Component
                )
                && candidate
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .is_some_and(|name| name == request.name)
        },
        cancelled,
    )?;
    Ok(match local {
        Some(candidate) => ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: EXACT_LEXICAL_CONFIDENCE,
            provenance: EXACT_LEXICAL_PROVENANCE,
        }),
        None => ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE),
    })
}

fn rust_self_has_local_nominal<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.dispatch != ReferenceDispatch::RustSelf {
        return Ok(false);
    }
    let Some((nominal, _)) = request
        .name
        .strip_prefix("self::")
        .and_then(|name| name.rsplit_once("::"))
    else {
        return Ok(false);
    };
    for candidate in resolution_candidates_for_file(index, nominal, request.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.file_id == *request.file_id
            && candidate.qualified_name == nominal
            && matches!(
                candidate.kind,
                SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Union | SymbolKind::TypeAlias
            )
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_rust_qualified_path<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != SourceLanguage::Rust.as_str() {
        return Ok(None);
    }
    let Some((module_specifier, name)) = request.name.rsplit_once("::") else {
        return Ok(None);
    };
    if !matches!(
        module_specifier.split("::").next(),
        Some("crate" | "self" | "super")
    ) && request.dispatch != ReferenceDispatch::RustSelf
    {
        return Ok(None);
    }
    if let Some(target) = resolve_rust_qualified_in_module(
        RustQualifiedModuleQuery {
            index,
            request,
            module_specifier,
            target_name: name,
            allow_unique_project_reexport: false,
        },
        cancelled,
    )? {
        return Ok(Some(target));
    }
    let Some((associated_module, parent_name)) = module_specifier.rsplit_once("::") else {
        return Ok(None);
    };
    if !matches!(
        associated_module.split("::").next(),
        Some("crate" | "self" | "super")
    ) && request.dispatch != ReferenceDispatch::RustSelf
    {
        return Ok(None);
    }
    let parent_and_member = request
        .name
        .strip_prefix(associated_module)
        .and_then(|suffix| suffix.strip_prefix("::"))
        .filter(|suffix| suffix.starts_with(parent_name))
        .ok_or(StageItemFailure)?;
    resolve_rust_qualified_in_module(
        RustQualifiedModuleQuery {
            index,
            request,
            module_specifier: associated_module,
            target_name: parent_and_member,
            allow_unique_project_reexport: associated_module == "crate"
                && request.dispatch != ReferenceDispatch::RustSelf,
        },
        cancelled,
    )
}

fn resolve_rust_qualified_in_module<Cancel>(
    query: RustQualifiedModuleQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let RustQualifiedModuleQuery {
        index,
        request,
        module_specifier,
        target_name,
        allow_unique_project_reexport,
    } = query;
    let Some(module_file_id) = resolve_module_file(
        &index.modules,
        ModuleResolutionRequest {
            importing_path: request.file_path,
            specifier: module_specifier,
            importing_language: request.language,
        },
    ) else {
        return Ok(None);
    };
    let candidates = resolution_candidates_for_file(index, target_name, module_file_id);
    let project_candidates = index.candidates.get(target_name).map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    let mut candidate = select_candidate(
        candidates,
        |candidate| {
            &candidate.file_id == module_file_id
                && rust_module_candidate_visible(RustCandidateVisibility {
                    index,
                    candidate,
                    target_name,
                    source_path: request.file_path,
                })
                && reference_kind_candidate(request.kind, candidate)
        },
        cancelled,
    )?;
    if candidate.is_none() && allow_unique_project_reexport {
        candidate = select_candidate(
            project_candidates,
            |candidate| {
                rust_module_candidate_visible(RustCandidateVisibility {
                    index,
                    candidate,
                    target_name,
                    source_path: request.file_path,
                }) && reference_kind_candidate(request.kind, candidate)
                    && index
                        .modules
                        .files
                        .get(&candidate.file_id)
                        .is_some_and(|target| {
                            resolution_languages_compatible(request.language, &target.language)
                        })
            },
            cancelled,
        )?;
    }
    Ok(candidate.map(|candidate| ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: IMPORT_BINDING_CONFIDENCE,
        provenance: RUST_QUALIFIED_PATH_PROVENANCE,
    }))
}

fn rust_module_candidate_visible(input: RustCandidateVisibility<'_>) -> bool {
    let RustCandidateVisibility {
        index,
        candidate,
        target_name,
        source_path,
    } = input;
    let Some(target) = index.modules.files.get(&candidate.file_id) else {
        return false;
    };
    let private_parent_visible = rust_parent_module_contains(source_path, &target.path);
    if !candidate.export.exported
        && candidate.kind != SymbolKind::EnumMember
        && !private_parent_visible
    {
        return false;
    }
    if candidate.parent_symbol_id.is_none() && candidate.top_level {
        return true;
    }
    let Some((parent_name, _)) = target_name.rsplit_once("::") else {
        return false;
    };
    if candidate.qualified_name != target_name {
        return false;
    }
    index.candidates.get(parent_name).is_some_and(|parents| {
        parents.iter().any(|parent| {
            parent.file_id == candidate.file_id
                && (parent.export.exported || private_parent_visible)
                && parent.top_level
                && candidate
                    .parent_symbol_id
                    .as_ref()
                    .is_none_or(|owner| owner == &parent.symbol_id)
        })
    })
}

fn is_lexical_candidate(
    reference_kind: ReferenceKind,
    reference_name: &str,
    candidate: &ResolutionCandidate,
) -> bool {
    reference_kind_candidate(reference_kind, candidate)
        && (reference_kind == ReferenceKind::FieldAccess
            || candidate.qualified_name == reference_name
            || !matches!(
                candidate.kind,
                SymbolKind::Method
                    | SymbolKind::Property
                    | SymbolKind::Field
                    | SymbolKind::EnumMember
            ))
}

/// Swift looks an unqualified call up among the enclosing type's methods
/// (implicit `self`) before outer scopes, so the lexical scope walk may stop at
/// a sibling method of the caller's own type.
fn swift_implicit_member_call(
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    request.language == SourceLanguage::Swift.as_str()
        && request.kind == ReferenceKind::Calls
        && candidate.kind == SymbolKind::Method
        && !request.name.contains('.')
        && !framework_landmark_candidate(candidate)
}

fn reference_kind_candidate(
    reference_kind: ReferenceKind,
    candidate: &ResolutionCandidate,
) -> bool {
    if candidate.augmentation {
        return false;
    }
    if reference_kind == ReferenceKind::Inherits {
        return matches!(
            candidate.kind,
            SymbolKind::Class | SymbolKind::Interface | SymbolKind::Struct | SymbolKind::Trait
        );
    }
    if matches!(
        reference_kind,
        ReferenceKind::TypeOf | ReferenceKind::Returns
    ) {
        return matches!(
            candidate.kind,
            SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Union
                | SymbolKind::Interface
                | SymbolKind::Trait
                | SymbolKind::Protocol
                | SymbolKind::Enum
                | SymbolKind::TypeAlias
        );
    }
    !(matches!(
        reference_kind,
        ReferenceKind::Calls | ReferenceKind::Instantiates
    ) && candidate.kind == SymbolKind::Module)
}

enum ImportResolution {
    NotBound,
    Resolved(ResolvedTarget),
    Unresolved,
}

fn resolve_module_import_file_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::Imports {
        return Ok(ImportResolution::NotBound);
    }
    if request.language == SourceLanguage::Python.as_str() {
        return python_resolution::resolve_module_reference(index, request, cancelled);
    }
    let mut bound = false;
    let mut matched = None;
    let mut default_only = true;
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !matches!(
            binding.kind,
            ImportBindingKind::Default | ImportBindingKind::Named | ImportBindingKind::Namespace
        ) || binding.module_specifier != request.name
            || binding.span != request.span
        {
            continue;
        }
        bound = true;
        default_only &= binding.kind == ImportBindingKind::Default;
        let Some(file_id) = resolve_module_file(
            &index.modules,
            ModuleResolutionRequest {
                importing_path: request.file_path,
                specifier: &binding.module_specifier,
                importing_language: request.language,
            },
        ) else {
            continue;
        };
        let file_symbol = index.file_symbols.get(file_id).ok_or(StageItemFailure)?;
        if matched
            .as_ref()
            .is_some_and(|candidate: &&SymbolId| *candidate != file_symbol)
        {
            return Ok(ImportResolution::Unresolved);
        }
        matched = Some(file_symbol);
    }
    match matched {
        Some(symbol_id) => {
            let mut target =
                component_imports::module_target(index, (request, default_only), cancelled)?
                    .unwrap_or(ResolvedTarget {
                        symbol_id: symbol_id.clone(),
                        kind: SymbolKind::File,
                        confidence: IMPORT_BINDING_CONFIDENCE,
                        provenance: MODULE_IMPORT_PROVENANCE,
                    });
            javascript_modules::lower_fallback_target(
                &mut target,
                &index.modules,
                ModuleResolutionRequest {
                    importing_path: request.file_path,
                    specifier: request.name,
                    importing_language: request.language,
                },
            );
            Ok(ImportResolution::Resolved(target))
        }
        None if bound => Ok(ImportResolution::Unresolved),
        None => Ok(ImportResolution::NotBound),
    }
}

fn resolve_include_file_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut matched: Option<SymbolId> = None;
    let mut bound = false;
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.span != request.span || binding.module_specifier != request.name {
            continue;
        }
        match binding.kind {
            ImportBindingKind::IncludeSystem => {
                bound = true;
            }
            ImportBindingKind::IncludeQuoted => {
                bound = true;
                let Some(file_id) = resolve_quoted_include_file(index, request, binding) else {
                    continue;
                };
                let file_symbol = index.file_symbols.get(file_id).ok_or(StageItemFailure)?;
                if matched
                    .as_ref()
                    .is_some_and(|candidate| candidate != file_symbol)
                {
                    return Ok(ImportResolution::Unresolved);
                }
                matched = Some(file_symbol.clone());
            }
            ImportBindingKind::Default
            | ImportBindingKind::Named
            | ImportBindingKind::Namespace
            | ImportBindingKind::ReExportAll
            | ImportBindingKind::ReExportUncertain
            | ImportBindingKind::ReExportNamespace
            | ImportBindingKind::ReExportNamed => {}
        }
    }
    match matched {
        Some(symbol_id) => Ok(ImportResolution::Resolved(ResolvedTarget {
            symbol_id,
            kind: SymbolKind::File,
            confidence: IMPORT_BINDING_CONFIDENCE,
            provenance: QUOTED_INCLUDE_PROVENANCE,
        })),
        None if bound => Ok(
            file_path_resolution::resolve_reference(index, request, cancelled)?
                .map_or(ImportResolution::Unresolved, ImportResolution::Resolved),
        ),
        None => Ok(ImportResolution::NotBound),
    }
}

fn resolve_include_bound_symbol<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !c_include_family_name(request.language) {
        return Ok(ImportResolution::NotBound);
    }
    let candidate_bucket = index.candidates.get(request.name);
    let candidates = candidate_bucket.map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    let declaration = match find_include_bound_candidate(IncludeBoundQuery {
        index,
        request,
        candidates,
        cancelled,
    })? {
        IncludeCandidateMatch::Absent => return Ok(ImportResolution::NotBound),
        IncludeCandidateMatch::Ambiguous => return Ok(ImportResolution::Unresolved),
        IncludeCandidateMatch::Unique(candidate) => candidate,
    };
    let target = if declaration.implementation.declaration_only {
        unique_include_implementation(
            IncludeImplementationQuery {
                index,
                request,
                declaration,
            },
            cancelled,
        )?
        .unwrap_or(declaration)
    } else {
        declaration
    };
    Ok(ImportResolution::Resolved(ResolvedTarget {
        symbol_id: target.symbol_id.clone(),
        kind: target.kind,
        confidence: IMPORT_BINDING_CONFIDENCE,
        provenance: QUOTED_INCLUDE_PROVENANCE,
    }))
}

enum IncludeCandidateMatch<'candidate> {
    Absent,
    Unique(&'candidate ResolutionCandidate),
    Ambiguous,
}

struct IncludeBoundQuery<'context, 'request, 'candidate, Cancel> {
    index: &'context ResolutionIndex,
    request: &'context ResolutionRequest<'request>,
    candidates: &'candidate [ResolutionCandidate],
    cancelled: &'context mut Cancel,
}

fn find_include_bound_candidate<'candidate, Cancel>(
    query: IncludeBoundQuery<'_, '_, 'candidate, Cancel>,
) -> Result<IncludeCandidateMatch<'candidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let IncludeBoundQuery {
        index,
        request,
        candidates,
        cancelled,
    } = query;
    let mut retained = None;
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind != ImportBindingKind::IncludeQuoted {
            continue;
        }
        match quoted_include_candidate(QuotedIncludeQuery {
            index,
            request,
            binding,
            candidates,
            cancelled: &mut *cancelled,
        })? {
            IncludeCandidateMatch::Absent => {}
            IncludeCandidateMatch::Ambiguous => return Ok(IncludeCandidateMatch::Ambiguous),
            IncludeCandidateMatch::Unique(candidate) => {
                if retained.is_some_and(|current: &ResolutionCandidate| {
                    current.symbol_id != candidate.symbol_id
                }) {
                    return Ok(IncludeCandidateMatch::Ambiguous);
                }
                retained = Some(candidate);
            }
        }
    }
    Ok(retained.map_or(IncludeCandidateMatch::Absent, IncludeCandidateMatch::Unique))
}

struct QuotedIncludeQuery<'context, 'request, 'candidate, Cancel> {
    index: &'context ResolutionIndex,
    request: &'context ResolutionRequest<'request>,
    binding: &'context ExtractedImportBinding,
    candidates: &'candidate [ResolutionCandidate],
    cancelled: &'context mut Cancel,
}

fn quoted_include_candidate<'candidate, Cancel>(
    query: QuotedIncludeQuery<'_, '_, 'candidate, Cancel>,
) -> Result<IncludeCandidateMatch<'candidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let QuotedIncludeQuery {
        index,
        request,
        binding,
        candidates,
        cancelled,
    } = query;
    let Some(file_id) = resolve_quoted_include_file(index, request, binding) else {
        return Ok(IncludeCandidateMatch::Absent);
    };
    let mut retained = None;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if &candidate.file_id != file_id
            || !c_candidate_is_externally_visible(candidate)
            || !reference_kind_candidate(request.kind, candidate)
        {
            continue;
        }
        if retained
            .is_some_and(|current: &ResolutionCandidate| current.symbol_id != candidate.symbol_id)
        {
            return Ok(IncludeCandidateMatch::Ambiguous);
        }
        retained = Some(candidate);
    }
    Ok(retained.map_or(IncludeCandidateMatch::Absent, IncludeCandidateMatch::Unique))
}

fn unique_include_implementation<'a, Cancel>(
    query: IncludeImplementationQuery<'a, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'a ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let IncludeImplementationQuery {
        index,
        request,
        declaration,
    } = query;
    let candidates = index.candidates.get(&declaration.qualified_name).map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    select_candidate(
        candidates,
        |candidate| {
            candidate.symbol_id != declaration.symbol_id
                && !candidate.implementation.declaration_only
                && candidate.export.exported
                && candidate.kind == declaration.kind
                && candidate.qualified_name == declaration.qualified_name
                && candidate.signature == declaration.signature
                && reference_kind_candidate(request.kind, candidate)
                && index
                    .modules
                    .files
                    .get(&candidate.file_id)
                    .is_some_and(|target| {
                        resolution_languages_compatible(request.language, &target.language)
                    })
        },
        cancelled,
    )
}

fn c_candidate_is_externally_visible(candidate: &ResolutionCandidate) -> bool {
    candidate.export.exported && matches!(candidate.visibility, None | Some(Visibility::Public))
}

fn resolve_quoted_include_file<'a>(
    index: &'a ResolutionIndex,
    request: &ResolutionRequest<'_>,
    binding: &ExtractedImportBinding,
) -> Option<&'a FileId> {
    let sibling = normalize_include_path(request.file_path, &binding.module_specifier)?;
    match module_file_match(
        index.modules.exact.get(&sibling),
        &index.modules.files,
        request.language,
    ) {
        ModuleFileMatch::Unique(file_id) => return Some(file_id),
        ModuleFileMatch::Ambiguous => return None,
        ModuleFileMatch::Missing => {}
    }
    let root = NormalizedPath::parse(binding.module_specifier.trim_start_matches("./")).ok()?;
    match module_file_match(
        index.modules.exact.get(root.as_str()),
        &index.modules.files,
        request.language,
    ) {
        ModuleFileMatch::Unique(file_id) => Some(file_id),
        ModuleFileMatch::Missing | ModuleFileMatch::Ambiguous => None,
    }
}

fn resolve_import<Cancel>(
    index: &ResolutionIndex,
    input: ImportResolutionRequest<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let reference = input.reference;
    let (binding, imported_name) =
        match match_import_bindings((Some(index), reference), input.site, cancelled)? {
            ImportBindingMatch::NotBound => return Ok(ImportResolution::NotBound),
            ImportBindingMatch::Ambiguous => return Ok(ImportResolution::Unresolved),
            ImportBindingMatch::Unique(binding, imported_name) => (binding, imported_name),
        };
    resolve_bound_import(
        index,
        BoundImportQuery {
            import: input,
            binding,
            imported_name,
        },
        cancelled,
    )
}

#[derive(Clone, Copy)]
struct BoundImportQuery<'a, 'b> {
    import: ImportResolutionRequest<'a, 'b>,
    binding: &'a ExtractedImportBinding,
    imported_name: &'a str,
}

/// Apply language-specific binding rules before the common module/symbol lookup.
fn resolve_bound_import<Cancel>(
    index: &ResolutionIndex,
    query: BoundImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let BoundImportQuery {
        import: input,
        binding,
        imported_name: _,
    } = query;
    let ImportResolutionRequest { reference, site } = input;
    if !rust_use_bindings::allows(index, (reference, binding), cancelled)? {
        return Ok(ImportResolution::NotBound);
    }
    if let Some(resolution) =
        rust_path_resolution::resolve_binding(index, (reference, binding), cancelled)?
    {
        return Ok(resolution);
    }
    if reference.language == SourceLanguage::Php.as_str() {
        return php_resolution::resolve_use_binding(
            php_resolution::PhpUseBinding {
                index,
                reference,
                binding,
                site,
            },
            cancelled,
        );
    }
    if reference.language == SourceLanguage::Python.as_str() {
        return python_resolution::resolve_import(
            python_resolution::ImportQuery {
                index,
                input,
                binding,
            },
            cancelled,
        );
    }
    if let Some(resolution) = salesforce_resolution::resolve_binding(
        salesforce_resolution::SalesforceBindingQuery {
            index,
            binding,
            reference,
        },
        cancelled,
    )? {
        return Ok(resolution);
    }
    resolve_bound_module_import(index, query, cancelled)
}

/// Resolve a selected binding's module, then its namespace fallback in the same order.
fn resolve_bound_module_import<Cancel>(
    index: &ResolutionIndex,
    query: BoundImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let BoundImportQuery {
        import: input,
        binding,
        imported_name,
    } = query;
    let reference = input.reference;
    if imported_name.is_empty() {
        return Ok(ImportResolution::Unresolved);
    }
    let module_file_id = resolve_module_file(
        &index.modules,
        ModuleResolutionRequest {
            importing_path: reference.file_path,
            specifier: &binding.module_specifier,
            importing_language: reference.language,
        },
    );
    if let Some(module_file_id) = module_file_id
        && let Some(target) = resolve_module_import(
            ModuleImportQuery {
                index,
                import: input,
                binding,
                imported_name,
                module_file_id,
            },
            cancelled,
        )?
    {
        return Ok(ImportResolution::Resolved(target));
    }
    if let Some(target) = resolve_rust_namespace_symbol_import(
        RustNamespaceImportQuery {
            index,
            reference,
            binding,
        },
        cancelled,
    )? {
        return Ok(ImportResolution::Resolved(target));
    }
    Ok(if module_file_id.is_some() {
        ImportResolution::Unresolved
    } else {
        missing_import_module_resolution(index, reference, binding)
    })
}

#[derive(Clone, Copy)]
struct ModuleImportQuery<'a, 'b> {
    index: &'a ResolutionIndex,
    import: ImportResolutionRequest<'a, 'b>,
    binding: &'a ExtractedImportBinding,
    imported_name: &'a str,
    module_file_id: &'a FileId,
}

/// Bind an import to the declaration the resolved module file exposes under
/// the imported name.
fn resolve_module_import<Cancel>(
    query: ModuleImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ModuleImportQuery {
        index,
        import,
        binding,
        imported_name,
        module_file_id,
    } = query;
    let candidates = import_resolution_candidates(ImportCandidatesQuery {
        index,
        binding,
        imported_name,
        module_file_id,
    });
    let filter = ImportCandidateFilter {
        index,
        reference: import.reference,
        imported_name,
        module_file_id,
        javascript_value_usage: javascript_value_import_usage(import),
    };
    let mut target =
        select_candidate(candidates, |candidate| filter.matches(candidate), cancelled)?
            .map(import_binding_target);
    if target
        .as_ref()
        .is_some_and(|target| target.kind == SymbolKind::Export)
        && javascript_family_name(import.reference.language)
        && javascript_alias_exports::is_alias(query)
    {
        target = javascript_alias_exports::resolve(query, target, cancelled)?;
    }
    if target.is_none() {
        target = javascript_exports::resolve_import(query, cancelled)?;
    }
    if let Some(target) = target.as_mut() {
        javascript_modules::lower_fallback_target(
            target,
            &index.modules,
            ModuleResolutionRequest {
                importing_path: import.reference.file_path,
                specifier: &binding.module_specifier,
                importing_language: import.reference.language,
            },
        );
    }
    Ok(target)
}

/// A JavaScript-family use of an imported name in a value position, which a
/// type-only export cannot satisfy.
fn javascript_value_import_usage(import: ImportResolutionRequest<'_, '_>) -> bool {
    matches!(import.site, ImportReferenceSite::Usage)
        && javascript_family_name(import.reference.language)
        && !matches!(
            import.reference.kind,
            ReferenceKind::TypeOf
                | ReferenceKind::Returns
                | ReferenceKind::Inherits
                | ReferenceKind::Implements
                | ReferenceKind::Extends
        )
}

fn import_binding_target(candidate: &ResolutionCandidate) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: IMPORT_BINDING_CONFIDENCE,
        provenance: IMPORT_BINDING_PROVENANCE,
    }
}

fn resolve_rust_namespace_symbol_import<Cancel>(
    query: RustNamespaceImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if query.reference.language != SourceLanguage::Rust.as_str()
        || query.binding.kind != ImportBindingKind::Namespace
    {
        return Ok(None);
    }
    if let Some(target) = resolve_rust_workspace_symbol_import(query, cancelled)? {
        return Ok(Some(target));
    }
    let RustNamespaceImportQuery {
        index,
        reference,
        binding,
    } = query;
    let Some((parent_specifier, imported_leaf)) = binding.module_specifier.rsplit_once("::") else {
        return Ok(None);
    };
    let Some(module_file_id) = resolve_module_file(
        &index.modules,
        ModuleResolutionRequest {
            importing_path: reference.file_path,
            specifier: parent_specifier,
            importing_language: reference.language,
        },
    ) else {
        return Ok(None);
    };
    let target_name = if imported_leaf == "*" && binding.local_name == "*" {
        try_clone_text(reference.name)?
    } else {
        let suffix = reference
            .name
            .strip_prefix(&binding.local_name)
            .filter(|suffix| suffix.is_empty() || suffix.starts_with("::"))
            .ok_or(StageItemFailure)?;
        rust_imported_target_name(imported_leaf, suffix)?
    };
    let candidates = resolution_candidates_for_file(index, &target_name, module_file_id);
    let project_candidates = index.candidates.get(&target_name).map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    let allow_unique_project_reexport =
        parent_specifier == "crate" && reference.dispatch != ReferenceDispatch::RustSelf;
    let mut candidate = select_candidate(
        candidates,
        |candidate| {
            &candidate.file_id == module_file_id
                && rust_use_bindings::visible(index, reference, candidate)
                && rust_module_candidate_visible(RustCandidateVisibility {
                    index,
                    candidate,
                    target_name: &target_name,
                    source_path: reference.file_path,
                })
                && reference_kind_candidate(reference.kind, candidate)
        },
        cancelled,
    )?;
    if candidate.is_none() && allow_unique_project_reexport {
        candidate = select_candidate(
            project_candidates,
            |candidate| {
                rust_use_bindings::visible(index, reference, candidate)
                    && rust_module_candidate_visible(RustCandidateVisibility {
                        index,
                        candidate,
                        target_name: &target_name,
                        source_path: reference.file_path,
                    })
                    && reference_kind_candidate(reference.kind, candidate)
                    && index
                        .modules
                        .files
                        .get(&candidate.file_id)
                        .is_some_and(|target| target.language == SourceLanguage::Rust.as_str())
            },
            cancelled,
        )?;
    }
    Ok(candidate.map(import_binding_target))
}

fn resolve_rust_workspace_symbol_import<Cancel>(
    query: RustNamespaceImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let RustNamespaceImportQuery {
        index,
        reference,
        binding,
    } = query;
    let Some((crate_name, public_name)) = binding.module_specifier.split_once("::") else {
        return Ok(None);
    };
    let Some(Some(package)) = index.modules.rust_packages.get(crate_name) else {
        return Ok(None);
    };
    let mut re_export_target: Option<&str> = None;
    for re_export in &index.rust_named_re_exports {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if re_export.source_file_id != package.entry_file_id || re_export.public_name != public_name
        {
            continue;
        }
        if re_export_target.is_some_and(|existing| existing != re_export.module_specifier.as_str())
        {
            return Ok(None);
        }
        re_export_target = Some(&re_export.module_specifier);
    }
    if let Some(re_export_target) = re_export_target {
        let Some(entry) = index.modules.files.get(&package.entry_file_id) else {
            return Err(StageItemFailure);
        };
        let target_request = ResolutionRequest {
            file_id: &package.entry_file_id,
            file_path: &entry.path,
            language: SourceLanguage::Rust.as_str(),
            import_bindings: ImportBindingSelection::empty(),
            owner: None,
            name: re_export_target,
            dispatch: ReferenceDispatch::Static,
            kind: reference.kind,
            span: reference.span,
        };
        if let Some(mut target) = resolve_rust_qualified_path(index, &target_request, cancelled)?
            && rust_reexport_target_is_public(index, &target, re_export_target)
        {
            target.confidence = IMPORT_BINDING_CONFIDENCE;
            target.provenance = RUST_WORKSPACE_CRATE_PROVENANCE;
            return Ok(Some(target));
        }
    }
    let candidates = index.candidates.get(public_name).map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    let candidate = select_candidate(
        candidates,
        |candidate| {
            rust_package_contains_candidate(index, package, candidate)
                && rust_workspace_inline_candidate_is_public(index, candidate, public_name)
                && reference_kind_candidate(reference.kind, candidate)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: IMPORT_BINDING_CONFIDENCE,
        provenance: RUST_WORKSPACE_CRATE_PROVENANCE,
    }))
}

fn rust_package_contains_candidate(
    index: &ResolutionIndex,
    package: &RustPackageRoot,
    candidate: &ResolutionCandidate,
) -> bool {
    let Some(file) = index.modules.files.get(&candidate.file_id) else {
        return false;
    };
    if package.directory.is_empty() {
        file.path.starts_with("src/")
    } else {
        file.path
            .strip_prefix(&package.directory)
            .is_some_and(|suffix| suffix.starts_with("/src/"))
    }
}

fn rust_reexport_target_is_public(
    index: &ResolutionIndex,
    target: &ResolvedTarget,
    source_path: &str,
) -> bool {
    let Some(name) = source_path.rsplit("::").next() else {
        return false;
    };
    index.candidates.get(name).is_some_and(|candidates| {
        let mut matching = candidates
            .iter()
            .filter(|candidate| candidate.symbol_id == target.symbol_id);
        matching.next().is_some_and(|candidate| {
            matching.next().is_none()
                && (candidate.export.exported || candidate.kind == SymbolKind::EnumMember)
        })
    })
}

fn rust_workspace_inline_candidate_is_public(
    index: &ResolutionIndex,
    candidate: &ResolutionCandidate,
    public_name: &str,
) -> bool {
    if candidate.qualified_name != public_name
        || (!candidate.export.exported && candidate.kind != SymbolKind::EnumMember)
    {
        return false;
    }
    let mut qualified_name = candidate.qualified_name.as_str();
    let mut parent_symbol_id = candidate.parent_symbol_id.as_ref();
    while let Some((parent_name, _)) = qualified_name.rsplit_once("::") {
        let Some(expected_parent_id) = parent_symbol_id else {
            return false;
        };
        let Some(parents) = index.candidates.get(parent_name) else {
            return false;
        };
        let mut matching = parents.iter().filter(|parent| {
            parent.file_id == candidate.file_id && parent.symbol_id == *expected_parent_id
        });
        let Some(parent) = matching.next() else {
            return false;
        };
        if matching.next().is_some() || !parent.export.exported {
            return false;
        }
        parent_symbol_id = parent.parent_symbol_id.as_ref();
        qualified_name = parent_name;
    }
    parent_symbol_id.is_none()
}

fn rust_imported_target_name(
    imported_leaf: &str,
    suffix: &str,
) -> Result<String, StageItemFailure> {
    let length = imported_leaf
        .len()
        .checked_add(suffix.len())
        .ok_or(StageItemFailure)?;
    let mut target = String::new();
    target
        .try_reserve_exact(length)
        .map_err(|_| StageItemFailure)?;
    target.push_str(imported_leaf);
    target.push_str(suffix);
    Ok(target)
}

enum ImportBindingMatch<'binding> {
    NotBound,
    Ambiguous,
    Unique(&'binding ExtractedImportBinding, &'binding str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportScope {
    None,
    Local,
    NonLocal,
    Ambiguous,
}

fn reference_import_scope<Cancel>(
    index: &ResolutionIndex,
    reference: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ImportScope, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut scope = ImportScope::None;
    for binding in reference.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !binding_matches_reference_name(binding, reference.name) {
            continue;
        }
        let candidate = if import_binding_is_project_local(index, binding, reference) {
            ImportScope::Local
        } else {
            ImportScope::NonLocal
        };
        if scope != ImportScope::None {
            return Ok(ImportScope::Ambiguous);
        }
        scope = candidate;
    }
    Ok(scope)
}

fn binding_matches_reference_name(binding: &ExtractedImportBinding, reference_name: &str) -> bool {
    binding.local_name == "*"
        || binding.local_name == reference_name
        || reference_name
            .strip_prefix(&binding.local_name)
            .is_some_and(|suffix| suffix.starts_with("::") || suffix.starts_with('.'))
}

fn import_binding_is_project_local(
    index: &ResolutionIndex,
    binding: &ExtractedImportBinding,
    reference: &ResolutionRequest<'_>,
) -> bool {
    let specifier = binding.module_specifier.as_str();
    import_specifier_is_explicitly_local(binding.kind, specifier)
        || language_import_is_project_local(index, specifier, reference)
}

fn import_specifier_is_explicitly_local(kind: ImportBindingKind, specifier: &str) -> bool {
    matches!(kind, ImportBindingKind::IncludeQuoted)
        || matches!(specifier, "." | "..")
        || specifier.starts_with("./")
        || specifier.starts_with("../")
}

fn language_import_is_project_local(
    index: &ResolutionIndex,
    specifier: &str,
    reference: &ResolutionRequest<'_>,
) -> bool {
    if reference.language == SourceLanguage::Rust.as_str()
        && rust_use_binding_is_hypothesis(specifier)
    {
        return true;
    }
    if SourceLanguage::from_stable_str(reference.language).is_some_and(|language| {
        language.is_game_scripting() && game_script_module_specifier_is_local(specifier)
    }) {
        return true;
    }
    if matches!(reference.language, "slang" | "wesl") {
        return true;
    }
    let module_request = ModuleResolutionRequest {
        importing_path: reference.file_path,
        specifier,
        importing_language: reference.language,
    };
    if compilation_unit_language(reference.language)
        && resolve_module_file(&index.modules, module_request).is_some()
    {
        return true;
    }
    if typescript_alias_matches(
        &index.modules,
        TypeScriptAliasMatch {
            importing_path: reference.file_path,
            specifier,
            importing_language: reference.language,
        },
    ) {
        return true;
    }
    javascript_modules::module_language(reference.language)
        && (javascript_alias_specifier(specifier)
            || javascript_packages::is_local(&index.modules, specifier)
            || resolve_framework_alias_file(&index.modules, module_request).is_some())
}

fn javascript_alias_specifier(specifier: &str) -> bool {
    specifier.starts_with("@/") || specifier.starts_with("~/") || specifier.starts_with("$lib/")
}

fn matched_import_binding<'binding, Cancel>(
    reference: &ResolutionRequest<'binding>,
    site: ImportReferenceSite,
    cancelled: &mut Cancel,
) -> Result<ImportBindingMatch<'binding>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    match_import_bindings((None, reference), site, cancelled)
}

fn match_import_bindings<'binding, Cancel>(
    query: (Option<&ResolutionIndex>, &ResolutionRequest<'binding>),
    site: ImportReferenceSite,
    cancelled: &mut Cancel,
) -> Result<ImportBindingMatch<'binding>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (index, reference) = query;
    let mut matched: Option<(&ExtractedImportBinding, &str)> = None;
    for binding in reference.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if reference.language == "rust"
            && binding.kind == ImportBindingKind::Namespace
            && index.is_some_and(|index| {
                !rust_local_types::root_declaration(index, (reference.file_id, binding.span))
            })
        {
            continue;
        }
        let imported_name = match site {
            ImportReferenceSite::Declaration => declaration_binding_target_name(binding, reference),
            ImportReferenceSite::Usage => {
                runtime_binding_target_name(binding, reference.name, reference.language)
            }
        };
        let Some(imported_name) = imported_name else {
            continue;
        };
        if matched.is_some() {
            return Ok(ImportBindingMatch::Ambiguous);
        }
        matched = Some((binding, imported_name));
    }
    Ok(match matched {
        Some((binding, imported_name)) => ImportBindingMatch::Unique(binding, imported_name),
        None => ImportBindingMatch::NotBound,
    })
}

fn missing_import_module_resolution(
    index: &ResolutionIndex,
    reference: &ResolutionRequest<'_>,
    binding: &ExtractedImportBinding,
) -> ImportResolution {
    if !import_binding_is_project_local(index, binding, reference)
        || (reference.language == SourceLanguage::Rust.as_str()
            && binding.kind == ImportBindingKind::Namespace
            && rust_use_binding_is_hypothesis(&binding.module_specifier))
    {
        ImportResolution::NotBound
    } else {
        ImportResolution::Unresolved
    }
}

fn import_resolution_candidates(query: ImportCandidatesQuery<'_>) -> &[ResolutionCandidate] {
    let ImportCandidatesQuery {
        index,
        binding,
        imported_name,
        module_file_id,
    } = query;
    if binding.kind == ImportBindingKind::Default || imported_name == "default" {
        index
            .default_exports
            .get(module_file_id)
            .map_or(&[] as &[ResolutionCandidate], Vec::as_slice)
    } else {
        resolution_candidates_for_file(index, imported_name, module_file_id)
    }
}

fn declaration_binding_target_name<'a>(
    binding: &'a ExtractedImportBinding,
    request: &ResolutionRequest<'_>,
) -> Option<&'a str> {
    if binding.span != request.span {
        return None;
    }
    if binding.kind == ImportBindingKind::Named && binding.imported_name == request.name {
        return Some(binding.imported_name.as_str());
    }
    (request.language == SourceLanguage::Rust.as_str()
        && binding.kind == ImportBindingKind::Namespace
        && binding.local_name == request.name)
        .then_some(binding.local_name.as_str())
}

fn runtime_binding_target_name<'a>(
    binding: &'a ExtractedImportBinding,
    reference_name: &'a str,
    language: &str,
) -> Option<&'a str> {
    match binding.kind {
        ImportBindingKind::Default | ImportBindingKind::Named => {
            if binding.local_name == reference_name {
                return Some(binding.imported_name.as_str());
            }
            reference_name
                .strip_prefix(&binding.local_name)
                .filter(|suffix| suffix.starts_with('.') || suffix.starts_with("::"))
                .map(|_| binding.imported_name.as_str())
        }
        ImportBindingKind::Namespace => {
            if matches!(language, "ada" | "vhdl" | "rust") && binding.local_name == "*" {
                return Some(reference_name);
            }
            if binding.local_name == reference_name {
                if language == SourceLanguage::Rust.as_str() {
                    return Some(binding.local_name.as_str());
                }
                return Some("");
            }
            reference_name
                .strip_prefix(&binding.local_name)
                .and_then(|suffix| {
                    suffix
                        .strip_prefix('.')
                        .or_else(|| suffix.strip_prefix("::"))
                })
        }
        ImportBindingKind::IncludeQuoted
        | ImportBindingKind::IncludeSystem
        | ImportBindingKind::ReExportAll
        | ImportBindingKind::ReExportUncertain
        | ImportBindingKind::ReExportNamespace
        | ImportBindingKind::ReExportNamed => None,
    }
}

fn javascript_runtime_import_candidate(candidate: &ResolutionCandidate) -> bool {
    !matches!(
        candidate.kind,
        SymbolKind::TypeAlias
            | SymbolKind::Interface
            | SymbolKind::Union
            | SymbolKind::Trait
            | SymbolKind::Protocol
    )
}

fn resolve_module_file<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> Option<&'a FileId> {
    match python_resolution::resolve_module_file(modules, request) {
        ModuleResolutionAttempt::Resolved(file_id) => return Some(file_id),
        ModuleResolutionAttempt::Rejected => return None,
        ModuleResolutionAttempt::NotMatched => {}
    }
    match script_modules::resolve_script_module(modules, request) {
        ModuleResolutionAttempt::Resolved(file_id) => return Some(file_id),
        ModuleResolutionAttempt::Rejected => return None,
        ModuleResolutionAttempt::NotMatched => {}
    }
    match resolve_compilation_unit_module(modules, request) {
        ModuleResolutionAttempt::Resolved(file_id) => return Some(file_id),
        ModuleResolutionAttempt::Rejected => return None,
        ModuleResolutionAttempt::NotMatched => {}
    }
    if let Some(file_id) = resolve_language_module_file(modules, request) {
        return Some(file_id);
    }
    if let Some(normalized) =
        normalize_relative_module_path(request.importing_path, request.specifier)
        && let Some(file_id) =
            resolve_normalized_module_file(modules, &normalized, request.importing_language)
    {
        return Some(file_id);
    }
    javascript_modules::resolve_alias_file(modules, request)
}

/// Resolve a specifier through the importing language's own module path
/// form (Slang root paths, WESL module paths, a Rust `crate` root or module
/// path, game-script resource paths) before any relative or alias lookup.
fn resolve_language_module_file<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> Option<&'a FileId> {
    let language = request.importing_language;
    if language == SourceLanguage::Wesl.as_str() {
        return resolve_wesl_module_file(modules, request);
    }
    if language == SourceLanguage::Rust.as_str()
        && request.specifier == "crate"
        && let Some(file_id) = rust_crate_entry_file(modules, request.importing_path)
    {
        return Some(file_id);
    }
    let normalized = language_normalized_module_path(modules, request)?;
    resolve_normalized_module_file(modules, &normalized, language)
}

/// The project-relative module path a language-specific specifier names, for
/// the languages whose specifiers are not plain relative paths.
fn language_normalized_module_path(
    modules: &ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> Option<String> {
    let language = request.importing_language;
    if language == SourceLanguage::Slang.as_str() {
        normalize_root_module_path(request.specifier)
    } else if language == SourceLanguage::Rust.as_str() {
        normalize_rust_module_path(modules, request.importing_path, request.specifier)
    } else if SourceLanguage::from_stable_str(language)
        .is_some_and(SourceLanguage::is_game_scripting)
    {
        normalize_game_script_module_path(request.importing_path, request.specifier)
    } else {
        None
    }
}

enum ModuleResolutionAttempt<'a> {
    NotMatched,
    Resolved(&'a FileId),
    Rejected,
}

fn resolve_compilation_unit_module<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    if !compilation_unit_language(request.importing_language) {
        return ModuleResolutionAttempt::NotMatched;
    }
    match compilation_unit_file(modules, request.specifier, request.importing_language) {
        ModuleFileMatch::Unique(file_id) => ModuleResolutionAttempt::Resolved(file_id),
        ModuleFileMatch::Ambiguous => ModuleResolutionAttempt::Rejected,
        ModuleFileMatch::Missing => ModuleResolutionAttempt::NotMatched,
    }
}

fn resolve_framework_alias_file<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> Option<&'a FileId> {
    match javascript_modules::conventional_file(modules, request) {
        ModuleResolutionAttempt::Resolved(file) => Some(file),
        ModuleResolutionAttempt::NotMatched | ModuleResolutionAttempt::Rejected => None,
    }
}

fn compilation_unit_file<'a>(
    modules: &'a ModulePathIndex,
    unit: &str,
    importing_language: &str,
) -> ModuleFileMatch<'a> {
    match module_file_match(
        modules.declared_units.get(unit),
        &modules.files,
        importing_language,
    ) {
        ModuleFileMatch::Missing => module_file_match(
            modules.implemented_units.get(unit),
            &modules.files,
            importing_language,
        ),
        selected => selected,
    }
}

fn resolve_typescript_alias_module<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> TypeScriptAliasModuleResolution<'a> {
    if !javascript_modules::module_language(request.importing_language)
        || matches!(request.specifier, "." | "..")
        || request.specifier.starts_with("./")
        || request.specifier.starts_with("../")
    {
        return TypeScriptAliasModuleResolution::NotMatched;
    }
    let Some(config) = nearest_typescript_alias_config(modules, request.importing_path) else {
        return TypeScriptAliasModuleResolution::NotMatched;
    };
    for mapping in &config.mappings {
        let Some(tail) = typescript_alias_tail(request.specifier, &mapping.pattern) else {
            continue;
        };
        for substitution in &mapping.substitutions {
            let replaced = substitute_module_alias(substitution, tail);
            let Some(candidate) = normalize_typescript_alias_target(&config.base_path, &replaced)
            else {
                continue;
            };
            match javascript_modules::resolve_normalized(
                modules,
                &candidate,
                request.importing_language,
            ) {
                ModuleResolutionAttempt::Resolved(file_id) => {
                    let fallback = javascript_modules::uses_cross_language_fallback(
                        modules,
                        &candidate,
                        request.importing_language,
                    );
                    return TypeScriptAliasModuleResolution::Resolved(file_id, fallback);
                }
                ModuleResolutionAttempt::Rejected => {
                    return TypeScriptAliasModuleResolution::Ambiguous;
                }
                ModuleResolutionAttempt::NotMatched => {}
            }
        }
        return TypeScriptAliasModuleResolution::Unresolved;
    }
    TypeScriptAliasModuleResolution::NotMatched
}

fn nearest_typescript_alias_config<'a>(
    modules: &'a ModulePathIndex,
    importing_path: &str,
) -> Option<&'a TypeScriptAliasConfig> {
    let mut directory = importing_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    loop {
        if let Some(config) = modules.typescript_aliases.by_directory.get(directory) {
            return Some(config);
        }
        let Some((parent, _)) = directory.rsplit_once('/') else {
            return if directory.is_empty() {
                None
            } else {
                modules.typescript_aliases.by_directory.get("")
            };
        };
        directory = parent;
    }
}

fn typescript_alias_tail<'a>(specifier: &'a str, pattern: &str) -> Option<&'a str> {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return (specifier == pattern).then_some("");
    };
    specifier.strip_circumfix(prefix, suffix)
}

fn normalize_typescript_alias_target(base_path: &str, substitution: &str) -> Option<String> {
    if base_path.is_empty() {
        return normalize_root_module_path(substitution);
    }
    let anchor = format!("{base_path}/__cartograph_alias__.ts");
    normalize_joined_project_path(&anchor, substitution)
}

#[derive(Clone, Copy)]
struct TypeScriptAliasMatch<'context> {
    importing_path: &'context str,
    specifier: &'context str,
    importing_language: &'context str,
}

fn typescript_alias_matches(modules: &ModulePathIndex, input: TypeScriptAliasMatch<'_>) -> bool {
    if !javascript_modules::module_language(input.importing_language) {
        return false;
    }
    nearest_typescript_alias_config(modules, input.importing_path).is_some_and(|config| {
        config
            .mappings
            .iter()
            .any(|mapping| typescript_alias_tail(input.specifier, &mapping.pattern).is_some())
    })
}

fn rust_use_binding_is_hypothesis(specifier: &str) -> bool {
    matches!(specifier, "." | ".." | "crate" | "self" | "super")
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with("crate::")
        || specifier.starts_with("self::")
        || specifier.starts_with("super::")
}

fn normalize_rust_module_path(
    modules: &ModulePathIndex,
    importing_path: &str,
    specifier: &str,
) -> Option<String> {
    if specifier.is_empty() || specifier.contains(['/', '\\', '\0']) {
        return None;
    }
    let mut components = specifier.split("::");
    let first = components.next()?;
    let mut normalized = match first {
        "crate" => rust_crate_module_directory(modules, importing_path)?,
        "self" => rust_current_module_directory(importing_path)?,
        "super" => {
            let mut directory = rust_current_module_directory(importing_path)?;
            pop_rust_module_directory(&mut directory)?;
            while components.clone().next() == Some("super") {
                components.next();
                pop_rust_module_directory(&mut directory)?;
            }
            directory
        }
        component => {
            let mut directory = rust_current_module_directory(importing_path)?;
            append_rust_module_component(&mut directory, component)?;
            directory
        }
    };
    append_rust_module_suffix(&mut normalized, components)?;
    (!normalized.is_empty()).then_some(normalized)
}

fn resolve_wesl_module_file<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> Option<&'a FileId> {
    let normalized = normalize_wesl_module_path(request.importing_path, request.specifier)?;
    if let Some(file_id) =
        resolve_normalized_module_file(modules, &normalized, request.importing_language)
    {
        return Some(file_id);
    }
    let parent = normalized.rsplit_once('/').map(|(parent, _)| parent)?;
    resolve_normalized_module_file(modules, parent, request.importing_language)
}

fn normalize_wesl_module_path(importing_path: &str, specifier: &str) -> Option<String> {
    if specifier.is_empty() || specifier.len() > 512 || specifier.contains(['/', '\\', '\0']) {
        return None;
    }
    let mut components = specifier.split("::").peekable();
    let first = components.next()?;
    let mut path = if first == "package" {
        String::new()
    } else if first == "super" {
        let mut directory = importing_path
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory)
            .to_owned();
        while components.peek() == Some(&"super") {
            components.next();
            if let Some((parent, _)) = directory.rsplit_once('/') {
                directory.truncate(parent.len());
            } else {
                directory.clear();
            }
        }
        directory
    } else {
        let mut root = String::new();
        append_wesl_component(&mut root, first)?;
        root
    };
    for component in components {
        if matches!(component, "package" | "super" | "*") {
            break;
        }
        append_wesl_component(&mut path, component)?;
    }
    (!path.is_empty()).then_some(path)
}

fn append_wesl_component(path: &mut String, component: &str) -> Option<()> {
    if component.is_empty()
        || !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    path.try_reserve_exact(component.len().saturating_add(1))
        .ok()?;
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(component);
    Some(())
}

fn append_rust_module_suffix<'component>(
    path: &mut String,
    components: impl Iterator<Item = &'component str>,
) -> Option<()> {
    for component in components {
        if component.is_empty() || matches!(component, "crate" | "self" | "super") {
            return None;
        }
        append_rust_module_component(path, component)?;
    }
    Some(())
}

fn rust_crate_module_directory(modules: &ModulePathIndex, importing_path: &str) -> Option<String> {
    let mut directory = importing_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    loop {
        if rust_crate_entry_in_directory(modules, directory) {
            return try_clone_text(directory).ok();
        }
        if directory.is_empty() {
            return None;
        }
        directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

fn rust_crate_entry_file<'a>(
    modules: &'a ModulePathIndex,
    importing_path: &str,
) -> Option<&'a FileId> {
    let directory = rust_crate_module_directory(modules, importing_path)?;
    let mut selected = None;
    for filename in ["lib.rs", "main.rs"] {
        let path = joined_path(&directory, filename).ok()?;
        let candidate = match module_file_match(
            modules.exact.get(&path),
            &modules.files,
            SourceLanguage::Rust.as_str(),
        ) {
            ModuleFileMatch::Unique(candidate) => candidate,
            ModuleFileMatch::Missing => continue,
            ModuleFileMatch::Ambiguous => return None,
        };
        if selected.is_some_and(|existing| existing != candidate) {
            return None;
        }
        selected = Some(candidate);
    }
    selected
}

fn rust_crate_entry_in_directory(modules: &ModulePathIndex, directory: &str) -> bool {
    ["lib.rs", "main.rs"].into_iter().any(|filename| {
        joined_path(directory, filename)
            .ok()
            .and_then(|path| modules.exact.get(&path))
            .is_some_and(|files| {
                files.iter().any(|file_id| {
                    modules
                        .files
                        .get(file_id)
                        .is_some_and(|file| file.language == SourceLanguage::Rust.as_str())
                })
            })
    })
}

fn rust_current_module_directory(importing_path: &str) -> Option<String> {
    let filename = importing_path
        .rsplit_once('/')
        .map_or(importing_path, |(_, filename)| filename);
    if matches!(filename, "lib.rs" | "main.rs" | "mod.rs") {
        let directory = importing_path
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
        return try_clone_text(directory).ok();
    }
    try_clone_text(importing_path.strip_suffix(".rs")?).ok()
}

fn pop_rust_module_directory(directory: &mut String) -> Option<()> {
    if directory.is_empty() {
        return None;
    }
    if let Some(separator) = directory.rfind('/') {
        directory.truncate(separator);
    } else {
        directory.clear();
    }
    Some(())
}

fn append_rust_module_component(path: &mut String, component: &str) -> Option<()> {
    let separator = usize::from(!path.is_empty());
    let additional = component.len().checked_add(separator)?;
    path.try_reserve_exact(additional).ok()?;
    if separator > 0 {
        path.push('/');
    }
    path.push_str(component);
    Some(())
}

fn resolve_normalized_module_file<'a>(
    modules: &'a ModulePathIndex,
    normalized: &str,
    importing_language: &str,
) -> Option<&'a FileId> {
    if javascript_modules::module_language(importing_language) {
        return match javascript_modules::resolve_normalized(modules, normalized, importing_language)
        {
            ModuleResolutionAttempt::Resolved(file) => Some(file),
            ModuleResolutionAttempt::NotMatched | ModuleResolutionAttempt::Rejected => None,
        };
    }
    match module_file_match(
        modules.exact.get(normalized),
        &modules.files,
        importing_language,
    ) {
        ModuleFileMatch::Unique(file_id) => return Some(file_id),
        ModuleFileMatch::Ambiguous => return None,
        ModuleFileMatch::Missing => {}
    }
    let stem = strip_any_module_extension(normalized).unwrap_or(normalized);
    let stem_match = module_file_match(modules.stem.get(stem), &modules.files, importing_language);
    let directory_match = module_file_match(
        modules.directory_index.get(normalized),
        &modules.files,
        importing_language,
    );
    match choose_module_file_match(
        stem_match,
        directory_match,
        importing_language == SourceLanguage::Rust.as_str(),
    ) {
        ModuleFileMatch::Unique(file_id) => Some(file_id),
        ModuleFileMatch::Missing | ModuleFileMatch::Ambiguous => None,
    }
}

fn normalize_root_module_path(path: &str) -> Option<String> {
    normalize_joined_project_path("__cartograph_root__.ts", path)
}

enum ModuleFileMatch<'a> {
    Missing,
    Unique(&'a FileId),
    Ambiguous,
}

fn choose_module_file_match<'a>(
    stem: ModuleFileMatch<'a>,
    directory: ModuleFileMatch<'a>,
    merge_conventions: bool,
) -> ModuleFileMatch<'a> {
    if !merge_conventions {
        return match stem {
            ModuleFileMatch::Missing => directory,
            selected => selected,
        };
    }
    match (stem, directory) {
        (ModuleFileMatch::Missing, selected) | (selected, ModuleFileMatch::Missing) => selected,
        (ModuleFileMatch::Ambiguous, _) | (_, ModuleFileMatch::Ambiguous) => {
            ModuleFileMatch::Ambiguous
        }
        (ModuleFileMatch::Unique(_), ModuleFileMatch::Unique(_)) => ModuleFileMatch::Ambiguous,
    }
}

fn module_file_match<'a>(
    candidates: Option<&'a Vec<FileId>>,
    files: &FileResolutionContextMap,
    importing_language: &str,
) -> ModuleFileMatch<'a> {
    let mut matched = candidates
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|file_id| {
            files.get(*file_id).is_some_and(|target| {
                resolution_languages_compatible(importing_language, &target.language)
            })
        });
    let Some(file_id) = matched.next() else {
        return ModuleFileMatch::Missing;
    };
    if matched.next().is_some() {
        ModuleFileMatch::Ambiguous
    } else {
        ModuleFileMatch::Unique(file_id)
    }
}

fn normalize_relative_module_path(importing_path: &str, specifier: &str) -> Option<String> {
    if !(matches!(specifier, "." | "..")
        || specifier.starts_with("./")
        || specifier.starts_with("../"))
        || specifier.contains(['\\', '\0'])
    {
        return None;
    }
    normalize_joined_project_path(importing_path, specifier)
}

fn normalize_game_script_module_path(importing_path: &str, specifier: &str) -> Option<String> {
    if let Some(root_path) = specifier.strip_prefix("res://") {
        return normalize_root_module_path(root_path);
    }
    if let Some(game_path) = specifier.strip_prefix("/Game/") {
        return normalize_root_module_path(game_path);
    }
    game_script_module_specifier_is_local(specifier)
        .then(|| normalize_joined_project_path(importing_path, specifier))?
}

fn game_script_module_specifier_is_local(specifier: &str) -> bool {
    !specifier.is_empty()
        && specifier.len() <= MAXIMUM_TYPESCRIPT_PATH_TEXT_BYTES
        && !specifier.contains(['\\', '\0'])
        && (!specifier.starts_with('/') || specifier.starts_with("/Game/"))
        && (!specifier.contains("://") || specifier.starts_with("res://"))
}

fn normalize_include_path(importing_path: &str, specifier: &str) -> Option<String> {
    if specifier.is_empty()
        || specifier.len() > 4_096
        || specifier.starts_with('/')
        || specifier.contains(['\\', '\0'])
    {
        return None;
    }
    normalize_joined_project_path(importing_path, specifier)
}

fn normalize_joined_project_path(importing_path: &str, specifier: &str) -> Option<String> {
    let directory = importing_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let component_capacity = directory
        .bytes()
        .filter(|byte| *byte == b'/')
        .count()
        .checked_add(specifier.bytes().filter(|byte| *byte == b'/').count())?
        .checked_add(2)?;
    let mut components = Vec::new();
    components.try_reserve(component_capacity).ok()?;
    components.extend(
        directory
            .split('/')
            .filter(|component| !component.is_empty()),
    );
    for component in specifier.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            value => components.push(value),
        }
    }
    if components.is_empty() {
        return None;
    }
    let output_bytes = components
        .iter()
        .try_fold(components.len().saturating_sub(1), |total, component| {
            total.checked_add(component.len())
        })?;
    let mut normalized = String::new();
    normalized.try_reserve(output_bytes).ok()?;
    for (index, component) in components.into_iter().enumerate() {
        if index != 0 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    Some(normalized)
}

fn resolve_project<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(target) = generic_resolution::resolve_casefold_project(index, request, cancelled)? {
        return Ok(Some(target));
    }
    let source = project_source_context(index, request)?;
    if let Some(target) = resolve_apple_bridge(AppleBridgeQuery {
        index,
        source,
        request,
        cancelled,
    })? {
        return Ok(Some(target));
    }
    let candidate_bucket = index.candidates.get(request.name);
    let candidates = candidate_bucket.map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    if c_include_family_name(request.language)
        && candidates.iter().any(|candidate| {
            candidate.qualified_name == request.name
                && !c_candidate_is_externally_visible(candidate)
        })
    {
        return Ok(None);
    }
    let rust_local_import = request.language == SourceLanguage::Rust.as_str()
        && reference_import_scope(index, request, cancelled)? == ImportScope::Local;
    if let Some(candidate_bucket) = candidate_bucket {
        return resolve_project_candidates(ProjectResolutionQuery {
            index,
            source,
            request,
            candidate_bucket,
            rust_local_import,
            cancelled,
        });
    }
    if php_route_source(source) {
        return resolve_php_route_fallback(PhpRouteFallbackQuery {
            index,
            source,
            request,
            cancelled,
        });
    }
    javascript_framework_resolution::resolve(index, request, cancelled)
}

struct ProjectResolutionQuery<'context, 'request, Cancel> {
    index: &'context ResolutionIndex,
    source: &'context ResolutionFileContext,
    request: &'context ResolutionRequest<'request>,
    candidate_bucket: &'context ResolutionCandidateBucket,
    rust_local_import: bool,
    cancelled: &'context mut Cancel,
}

fn resolve_project_candidates<Cancel>(
    mut query: ProjectResolutionQuery<'_, '_, Cancel>,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(resolution) = javascript_member_resolution::resolve_project(&mut query)? {
        return Ok(resolution.target);
    }
    let ProjectResolutionQuery {
        index,
        source,
        request,
        candidate_bucket,
        rust_local_import,
        cancelled,
    } = query;
    let candidate = select_candidate(
        ProjectResolutionCandidates::new(candidate_bucket, source, request.name)?,
        |candidate| {
            is_project_candidate(ProjectCandidateInput {
                modules: &index.modules,
                source,
                source_file_id: request.file_id,
                reference_name: request.name,
                dynamic_dispatch: request.dispatch == ReferenceDispatch::Dynamic,
                rust_local_import,
                candidate,
            }) && reference_kind_candidate(request.kind, candidate)
        },
        cancelled,
    )?;
    if let Some(candidate) = candidate {
        return Ok(Some(project_resolved_target(candidate)));
    }
    let candidate = select_framework_candidate(FrameworkSelection {
        index,
        source,
        request,
        candidates: ProjectResolutionCandidates::new(candidate_bucket, source, request.name)?,
        cancelled,
    })?;
    if let Some(candidate) = candidate {
        return Ok(Some(framework_convention_target(candidate)));
    }
    generic_resolution::resolve_proximity(index, request, cancelled)
}

struct PhpRouteFallbackQuery<'context, 'request, Cancel> {
    index: &'context ResolutionIndex,
    source: &'context ResolutionFileContext,
    request: &'context ResolutionRequest<'request>,
    cancelled: &'context mut Cancel,
}

fn resolve_php_route_fallback<Cancel>(
    query: PhpRouteFallbackQuery<'_, '_, Cancel>,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let PhpRouteFallbackQuery {
        index,
        source,
        request,
        cancelled,
    } = query;
    for fallback_name in php_route_resolution_fallbacks(request.name)
        .into_iter()
        .flatten()
    {
        if fallback_name == request.name {
            continue;
        }
        let Some(candidates) = index.candidates.get(&fallback_name) else {
            continue;
        };
        let fallback_request = ResolutionRequest {
            file_id: request.file_id,
            file_path: request.file_path,
            language: request.language,
            import_bindings: request.import_bindings,
            owner: request.owner,
            name: &fallback_name,
            dispatch: request.dispatch,
            kind: request.kind,
            span: request.span,
        };
        let candidate = select_framework_candidate(FrameworkSelection {
            index,
            source,
            request: &fallback_request,
            candidates: ProjectResolutionCandidates::new(candidates, source, &fallback_name)?,
            cancelled,
        })?;
        return Ok(candidate.map(framework_convention_target));
    }
    Ok(None)
}

fn resolve_apple_bridge<Cancel>(
    input: AppleBridgeQuery<'_, '_, Cancel>,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let AppleBridgeQuery {
        index,
        source,
        request,
        cancelled,
    } = input;
    let target_language = match source.language.as_str() {
        "swift" => "objc",
        "objc" => "swift",
        _ => return Ok(None),
    };
    let direct_name = request.name.rsplit('.').next().unwrap_or(request.name);
    let direct = index.candidates.get(direct_name).map_or(
        &[] as &[ResolutionCandidate],
        ResolutionCandidateBucket::as_slice,
    );
    if source.language == "objc" && direct_name.contains(':') {
        // Objective-C methods are named by their full selector, so a keyword
        // send names its native implementation exactly.
        match select_objc_selector_method(ObjcSelectorQuery {
            index,
            request,
            candidates: direct,
            cancelled: &mut *cancelled,
        })? {
            ObjcSelectorMatch::Unique(candidate) => {
                return Ok(Some(project_resolved_target(candidate)));
            }
            ObjcSelectorMatch::Ambiguous => return Ok(None),
            ObjcSelectorMatch::Absent => {}
        }
    } else if direct.iter().any(|candidate| {
        index
            .modules
            .files
            .get(&candidate.file_id)
            .is_some_and(|file| {
                file.language == source.language
                    && candidate.visibility != Some(Visibility::Private)
                    && reference_kind_candidate(request.kind, candidate)
            })
    }) {
        return Ok(None);
    }
    if source.language == "swift" {
        return select_apple_bridge_candidate(AppleBridgeSelection {
            index,
            source,
            request,
            candidates: direct,
            target_language,
            cancelled,
        })
        .map(|candidate| candidate.map(apple_bridge_target));
    }
    for candidate_name in swift_base_names_for_objc_selector(request.name) {
        let Some(candidate_name) = candidate_name else {
            continue;
        };
        let candidates = index.candidates.get(&candidate_name).map_or(
            &[] as &[ResolutionCandidate],
            ResolutionCandidateBucket::as_slice,
        );
        if let Some(candidate) = select_apple_bridge_candidate(AppleBridgeSelection {
            index,
            source,
            request,
            candidates,
            target_language,
            cancelled,
        })? {
            return Ok(Some(apple_bridge_target(candidate)));
        }
    }
    Ok(None)
}

/// How an Objective-C keyword selector matched native method declarations.
enum ObjcSelectorMatch<'candidate> {
    Absent,
    Ambiguous,
    Unique(&'candidate ResolutionCandidate),
}

struct ObjcSelectorQuery<'context, 'request, 'candidate, Cancel> {
    index: &'context ResolutionIndex,
    request: &'context ResolutionRequest<'request>,
    candidates: &'candidate [ResolutionCandidate],
    cancelled: &'context mut Cancel,
}

/// Which Objective-C method declarations one selector lookup pass considers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ObjcMethodRole {
    /// Methods a class (or its categories) declares or implements.
    ClassMember,
    /// Requirements a protocol declares for its conforming classes.
    ProtocolRequirement,
}

/// The one non-private Objective-C method (never a bridge alias) declaring the
/// sent selector. Class members are considered first: a definition wins over
/// header declarations of the same class's method, and declarations of the
/// selector in two classes abstain because the receiver's class is unknown. A
/// protocol requirement never competes with them, since conforming classes
/// implement it; it is the target only when no class declares the selector and
/// exactly one protocol requires it.
fn select_objc_selector_method<'candidate, Cancel>(
    query: ObjcSelectorQuery<'_, '_, 'candidate, Cancel>,
) -> Result<ObjcSelectorMatch<'candidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ObjcSelectorQuery {
        index,
        request,
        candidates,
        cancelled,
    } = query;
    for role in [
        ObjcMethodRole::ClassMember,
        ObjcMethodRole::ProtocolRequirement,
    ] {
        let selected = select_objc_selector_role(
            ObjcSelectorQuery {
                index,
                request,
                candidates,
                cancelled: &mut *cancelled,
            },
            role,
        )?;
        if !matches!(selected, ObjcSelectorMatch::Absent) {
            return Ok(selected);
        }
    }
    Ok(ObjcSelectorMatch::Absent)
}

/// One [`select_objc_selector_method`] pass over the methods playing `role`:
/// absent when none declares the selector, ambiguous when two containers do.
fn select_objc_selector_role<'candidate, Cancel>(
    query: ObjcSelectorQuery<'_, '_, 'candidate, Cancel>,
    role: ObjcMethodRole,
) -> Result<ObjcSelectorMatch<'candidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ObjcSelectorQuery {
        index,
        request,
        candidates,
        cancelled,
    } = query;
    let mut eligible = |candidate: &ResolutionCandidate| {
        objc_native_method(index, request, candidate) == Some(role)
    };
    let mut declaring_method: Option<&str> = None;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !eligible(candidate) {
            continue;
        }
        match declaring_method {
            None => declaring_method = Some(&candidate.qualified_name),
            Some(method) if method != candidate.qualified_name => {
                return Ok(ObjcSelectorMatch::Ambiguous);
            }
            Some(_) => {}
        }
    }
    if declaring_method.is_none() {
        return Ok(ObjcSelectorMatch::Absent);
    }
    let exact = select_candidate(candidates, &mut eligible, cancelled)?;
    Ok(exact.map_or(ObjcSelectorMatch::Ambiguous, ObjcSelectorMatch::Unique))
}

/// The role of a non-private, non-landmark Objective-C method candidate the
/// reference may target, or `None` for any other candidate.
fn objc_native_method(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> Option<ObjcMethodRole> {
    let method = index
        .modules
        .files
        .get(&candidate.file_id)
        .is_some_and(|target| {
            target.language == SourceLanguage::ObjectiveC.as_str()
                && candidate.kind == SymbolKind::Method
                && candidate.visibility != Some(Visibility::Private)
                && !framework_landmark_candidate(candidate)
                && reference_kind_candidate(request.kind, candidate)
        });
    if !method {
        return None;
    }
    Some(if objc_protocol_member(index, candidate) {
        ObjcMethodRole::ProtocolRequirement
    } else {
        ObjcMethodRole::ClassMember
    })
}

/// Whether an Objective-C method's container is a protocol. Classes and
/// protocols are always top-level, so the container's qualified name is the
/// method's first qualified-name segment (a selector may itself contain `::`).
fn objc_protocol_member(index: &ResolutionIndex, candidate: &ResolutionCandidate) -> bool {
    let (Some(parent_id), Some((container, _)), Some(file_ordinal)) = (
        candidate.parent_symbol_id.as_ref(),
        candidate.qualified_name.split_once("::"),
        index.file_ordinals.get(&candidate.file_id),
    ) else {
        return false;
    };
    index.candidates.get(container).is_some_and(|containers| {
        containers
            .for_file(*file_ordinal)
            .iter()
            .any(|parent| &parent.symbol_id == parent_id && parent.kind == SymbolKind::Protocol)
    })
}

fn select_apple_bridge_candidate<'candidate, Cancel>(
    input: AppleBridgeSelection<'_, '_, 'candidate, Cancel>,
) -> Result<Option<&'candidate ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let AppleBridgeSelection {
        index,
        source,
        request,
        candidates,
        target_language,
        cancelled,
    } = input;
    select_candidate(
        candidates,
        |candidate| {
            index
                .modules
                .files
                .get(&candidate.file_id)
                .is_some_and(|target| {
                    target.language == target_language
                        && reference_kind_candidate(request.kind, candidate)
                        && is_framework_candidate(FrameworkCandidateInput {
                            modules: &index.modules,
                            source,
                            source_file_id: request.file_id,
                            candidate,
                        })
                })
        },
        cancelled,
    )
}

fn apple_bridge_target(candidate: &ResolutionCandidate) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
        provenance: APPLE_BRIDGE_PROVENANCE,
    }
}

fn select_framework_candidate<'candidate, Candidates, Cancel>(
    input: FrameworkSelection<'_, '_, Candidates, Cancel>,
) -> Result<Option<&'candidate ResolutionCandidate>, StageItemFailure>
where
    Candidates: IntoIterator<Item = &'candidate ResolutionCandidate>,
    Cancel: FnMut() -> bool,
{
    let FrameworkSelection {
        index,
        source,
        request,
        candidates,
        cancelled,
    } = input;
    let mut selected = None;
    let mut best_score = 0_u8;
    let mut best_count = 0_usize;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(target) = index.modules.files.get(&candidate.file_id) else {
            return Err(StageItemFailure);
        };
        let score = framework_convention_score(FrameworkConventionInput {
            reference_name: request.name,
            source,
            target,
            candidate,
        });
        let convention_eligible = is_framework_candidate(FrameworkCandidateInput {
            modules: &index.modules,
            source,
            source_file_id: request.file_id,
            candidate,
        }) || php_route_candidate(PhpRouteCandidateInput {
            source,
            target,
            source_file_id: request.file_id,
            candidate,
        });
        if score == 0 || !convention_eligible || !reference_kind_candidate(request.kind, candidate)
        {
            continue;
        }
        if score > best_score {
            selected = Some(candidate);
            best_score = score;
            best_count = 1;
        } else if score == best_score {
            best_count = best_count.saturating_add(1);
        }
    }
    Ok((best_count == 1).then_some(selected).flatten())
}

fn project_source_context<'a>(
    index: &'a ResolutionIndex,
    request: &ResolutionRequest<'_>,
) -> Result<&'a ResolutionFileContext, StageItemFailure> {
    let source = index
        .modules
        .files
        .get(request.file_id)
        .ok_or(StageItemFailure)?;
    (source.language == request.language)
        .then_some(source)
        .ok_or(StageItemFailure)
}

fn is_project_candidate(input: ProjectCandidateInput<'_>) -> bool {
    let Some(target) = input.modules.files.get(&input.candidate.file_id) else {
        return false;
    };
    let framework_bridge =
        framework_bridge_allowed(&input.source.language, &target.language, input.candidate);
    if &input.candidate.file_id == input.source_file_id {
        return false;
    }
    if !project_candidate_externally_visible(&input, target, framework_bridge) {
        return false;
    }
    if c_include_family_name(&input.source.language)
        && !c_candidate_is_externally_visible(input.candidate)
    {
        return false;
    }
    if !project_scope_matches(input.source, target, input.candidate) {
        return false;
    }
    !project_member_excluded(&input, framework_bridge)
}

fn project_member_excluded(input: &ProjectCandidateInput<'_>, framework_bridge: bool) -> bool {
    if input.dynamic_dispatch || framework_bridge {
        return false;
    }
    let member = matches!(
        input.candidate.kind,
        SymbolKind::Method
            | SymbolKind::Property
            | SymbolKind::Field
            | SymbolKind::EnumMember
            | SymbolKind::Parameter
    );
    member && input.candidate.qualified_name != input.reference_name
}

fn project_candidate_externally_visible(
    input: &ProjectCandidateInput<'_>,
    target: &ResolutionFileContext,
    framework_bridge: bool,
) -> bool {
    input.candidate.export.exported
        || input.candidate.visibility == Some(Visibility::Public)
        || rust_private_parent_visible(input, target)
        || (framework_bridge && input.candidate.visibility != Some(Visibility::Private))
}

fn rust_private_parent_visible(
    input: &ProjectCandidateInput<'_>,
    target: &ResolutionFileContext,
) -> bool {
    input.rust_local_import
        && input.source.language == SourceLanguage::Rust.as_str()
        && target.language == SourceLanguage::Rust.as_str()
        && rust_parent_module_contains(&input.source.path, &target.path)
}

fn rust_parent_module_contains(source_path: &str, target_path: &str) -> bool {
    let (directory, filename) = target_path
        .rsplit_once('/')
        .map_or(("", target_path), |(directory, filename)| {
            (directory, filename)
        });
    let module = if matches!(filename, "lib.rs" | "main.rs" | "mod.rs") {
        directory
    } else {
        let Some(stem) = target_path.strip_suffix(".rs") else {
            return false;
        };
        stem
    };
    if module.is_empty() {
        return true;
    }
    source_path
        .strip_prefix(module)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_framework_candidate(input: FrameworkCandidateInput<'_>) -> bool {
    let FrameworkCandidateInput {
        modules,
        source,
        source_file_id,
        candidate,
    } = input;
    let Some(target) = modules.files.get(&candidate.file_id) else {
        return false;
    };
    &candidate.file_id != source_file_id
        && framework_conventions::visible(source, target, candidate)
        && (!c_include_family_name(&source.language)
            || c_candidate_is_externally_visible(candidate))
        && project_scope_matches(source, target, candidate)
}

fn project_resolved_target(candidate: &ResolutionCandidate) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: EXACT_PROJECT_CONFIDENCE,
        provenance: EXACT_PROJECT_PROVENANCE,
    }
}

fn framework_convention_target(candidate: &ResolutionCandidate) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
        provenance: FRAMEWORK_CONVENTION_PROVENANCE,
    }
}

const FRAMEWORK_SCORE_OBJC_BRIDGE: u8 = 125;
const FRAMEWORK_SCORE_TURBO_MODULE: u8 = 124;
const FRAMEWORK_SCORE_SWIFT_BRIDGE: u8 = 123;
const FRAMEWORK_SCORE_JVM_BRIDGE: u8 = 120;
const FRAMEWORK_SCORE_APPLE_BRIDGE: u8 = 115;
const FRAMEWORK_SCORE_LOCAL_COMPONENT: u8 = 112;
const FRAMEWORK_SCORE_ROUTE_METHOD: u8 = 110;
const FRAMEWORK_SCORE_CONVENTIONAL_COMPONENT: u8 = 105;
const FRAMEWORK_SCORE_STRONG: u8 = 100;
const FRAMEWORK_SCORE_NAMED_CONVENTION: u8 = 95;
const FRAMEWORK_SCORE_EXACT_DIRECTORY: u8 = 90;
/// A name and declaration-kind convention with no directory evidence.
const FRAMEWORK_SCORE_KIND_CONVENTION: u8 = 85;
const FRAMEWORK_SCORE_COMPONENT_FALLBACK: u8 = 80;
const FRAMEWORK_SCORE_PHP_MODEL: u8 = 75;
const FRAMEWORK_SCORE_MODEL: u8 = 70;
const FRAMEWORK_SCORE_PASCAL_FALLBACK: u8 = 10;

#[derive(Clone, Copy)]
struct FrameworkConventionInput<'a> {
    reference_name: &'a str,
    source: &'a ResolutionFileContext,
    target: &'a ResolutionFileContext,
    candidate: &'a ResolutionCandidate,
}

impl FrameworkConventionInput<'_> {
    fn same_directory(&self) -> bool {
        self.source.directory == self.target.directory
    }
}

enum FrameworkNamePattern {
    Prefixes {
        values: &'static [&'static str],
        minimum_length: usize,
    },
    Suffixes(&'static [&'static str]),
    ExactOrSuffixes {
        exact: &'static [&'static str],
        suffixes: &'static [&'static str],
    },
    PrefixesOrSuffixes {
        prefixes: &'static [&'static str],
        suffixes: &'static [&'static str],
        minimum_length: usize,
    },
    PascalCase,
    Middleware,
}

impl FrameworkNamePattern {
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Prefixes {
                values,
                minimum_length,
            } => name.len() >= *minimum_length && name_has_any_prefix(name, values),
            Self::Suffixes(values) => name_has_any_suffix(name, values),
            Self::ExactOrSuffixes { exact, suffixes } => {
                exact.contains(&name) || name_has_any_suffix(name, suffixes)
            }
            Self::PrefixesOrSuffixes {
                prefixes,
                suffixes,
                minimum_length,
            } => {
                name.len() >= *minimum_length
                    && (name_has_any_prefix(name, prefixes) || name_has_any_suffix(name, suffixes))
            }
            Self::PascalCase => framework_pascal_case(name),
            Self::Middleware => is_middleware_convention(name),
        }
    }
}

fn name_has_any_prefix(name: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| name.starts_with(prefix))
}

fn name_has_any_suffix(name: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|suffix| name.ends_with(suffix))
}

enum FrameworkCandidatePattern {
    Any,
    Callable,
    TopLevelType,
    ClassOrInterface,
    Kinds(&'static [SymbolKind]),
    /// One of the kinds, declared at file scope: a nested type is unreachable
    /// by its bare name from another file. Component landmarks qualify.
    TopLevelKinds(&'static [SymbolKind]),
}

/// The kinds [`FrameworkCandidatePattern::TopLevelType`] admits.
const FRAMEWORK_TYPE_KINDS: &[SymbolKind] = &[
    SymbolKind::Class,
    SymbolKind::Struct,
    SymbolKind::Interface,
    SymbolKind::Component,
    SymbolKind::Module,
];

impl FrameworkCandidatePattern {
    fn matches(&self, candidate: &ResolutionCandidate) -> bool {
        match self {
            Self::Any => true,
            Self::Callable => framework_callable(candidate),
            Self::TopLevelType => framework_top_level_type(candidate),
            Self::ClassOrInterface => {
                matches!(candidate.kind, SymbolKind::Class | SymbolKind::Interface)
            }
            Self::Kinds(kinds) => kinds.contains(&candidate.kind),
            Self::TopLevelKinds(kinds) => {
                kinds.contains(&candidate.kind)
                    && (candidate.top_level || candidate.kind == SymbolKind::Component)
            }
        }
    }
}

struct FrameworkRule {
    name: FrameworkNamePattern,
    candidate: FrameworkCandidatePattern,
    /// Conventional directories; [`ANY_DIRECTORY`] makes the rule kind-only.
    directories: &'static [&'static str],
    score: u8,
}

/// A rule whose name and candidate kind are evidence enough in any directory.
const ANY_DIRECTORY: &[&str] = &[];

impl FrameworkRule {
    fn matches(&self, input: &FrameworkConventionInput<'_>) -> bool {
        self.name.matches(input.reference_name)
            && self.candidate.matches(input.candidate)
            && (self.directories.is_empty()
                || directory_has_any(&input.target.path, self.directories))
    }
}

const JAVASCRIPT_HOOK_MINIMUM_LENGTH: usize = 4;

const JAVASCRIPT_FRAMEWORK_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::Prefixes {
            values: &["use"],
            minimum_length: JAVASCRIPT_HOOK_MINIMUM_LENGTH,
        },
        candidate: FrameworkCandidatePattern::Callable,
        directories: &["hooks"],
        score: FRAMEWORK_SCORE_STRONG,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Context", "Provider"]),
        candidate: FrameworkCandidatePattern::Any,
        directories: &["context", "contexts", "providers"],
        score: FRAMEWORK_SCORE_NAMED_CONVENTION,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Middleware,
        candidate: FrameworkCandidatePattern::Callable,
        directories: &["middleware", "middlewares"],
        score: FRAMEWORK_SCORE_NAMED_CONVENTION,
    },
];

const JVM_FRAMEWORK_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Service"]),
        candidate: FrameworkCandidatePattern::ClassOrInterface,
        directories: &["service", "services"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Repository"]),
        candidate: FrameworkCandidatePattern::ClassOrInterface,
        directories: &["repository", "repositories"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Controller"]),
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["controller", "controllers"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Config", "Component"]),
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["config", "component", "components"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PascalCase,
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["entity", "entities", "model", "models", "domain"],
        score: FRAMEWORK_SCORE_MODEL,
    },
];

const PHP_FRAMEWORK_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Controller"]),
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["controller", "controllers"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PascalCase,
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["model", "models", "entity", "entities"],
        score: FRAMEWORK_SCORE_PHP_MODEL,
    },
];

const CSHARP_FRAMEWORK_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Controller"]),
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["controllers"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PrefixesOrSuffixes {
            prefixes: &["I"],
            suffixes: &["Service"],
            minimum_length: 2,
        },
        candidate: FrameworkCandidatePattern::ClassOrInterface,
        directories: &["services", "service", "application"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Repository"]),
        candidate: FrameworkCandidatePattern::ClassOrInterface,
        directories: &["repositories", "repository", "data", "infrastructure"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PascalCase,
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &[
            "models",
            "model",
            "entities",
            "entity",
            "domain",
            "viewmodels",
            "dtos",
        ],
        score: FRAMEWORK_SCORE_MODEL,
    },
];

const PYTHON_FRAMEWORK_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["View", "ViewSet"]),
        candidate: FrameworkCandidatePattern::Kinds(&[SymbolKind::Class, SymbolKind::Function]),
        directories: &["views"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Form"]),
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["forms"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::ExactOrSuffixes {
            exact: &["router"],
            suffixes: &["_router"],
        },
        candidate: FrameworkCandidatePattern::Kinds(&[SymbolKind::Variable, SymbolKind::Constant]),
        directories: &["routers", "api", "routes", "endpoints"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Prefixes {
            values: &["get_", "Depends"],
            minimum_length: 0,
        },
        candidate: FrameworkCandidatePattern::Callable,
        directories: &["dependencies", "deps", "core"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PascalCase,
        candidate: FrameworkCandidatePattern::TopLevelType,
        directories: &["models", "model"],
        score: FRAMEWORK_SCORE_MODEL,
    },
];

/// `SwiftUI`, `UIKit`, and Vapor naming conventions. A `ViewController` rule must
/// precede the Vapor `Controller` rule that its suffix also matches.
const SWIFT_FRAMEWORK_RULES: &[FrameworkRule] = &[
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["ViewController"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(FRAMEWORK_TYPE_KINDS),
        directories: &[
            "viewcontrollers",
            "viewcontroller",
            "controllers",
            "screens",
        ],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["View"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[
            SymbolKind::Component,
            SymbolKind::Class,
            SymbolKind::Struct,
        ]),
        directories: &["views", "view", "screens", "components", "ui"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Cell"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[SymbolKind::Class]),
        directories: &[
            "cells",
            "cell",
            "views",
            "tableviewcells",
            "collectionviewcells",
        ],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Delegate", "DataSource"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[
            SymbolKind::Interface,
            SymbolKind::Protocol,
        ]),
        directories: ANY_DIRECTORY,
        score: FRAMEWORK_SCORE_KIND_CONVENTION,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["ViewModel", "Store", "Manager"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(FRAMEWORK_TYPE_KINDS),
        directories: &["viewmodels", "viewmodel", "stores", "managers", "services"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Controller"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[
            SymbolKind::Class,
            SymbolKind::Struct,
        ]),
        directories: &["controllers", "controller", "routes"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::Suffixes(&["Middleware"]),
        candidate: FrameworkCandidatePattern::TopLevelKinds(&[
            SymbolKind::Class,
            SymbolKind::Struct,
        ]),
        directories: &["middleware", "middlewares"],
        score: FRAMEWORK_SCORE_EXACT_DIRECTORY,
    },
    FrameworkRule {
        name: FrameworkNamePattern::PascalCase,
        candidate: FrameworkCandidatePattern::TopLevelKinds(FRAMEWORK_TYPE_KINDS),
        directories: &["models", "model", "entities", "domain", "database"],
        score: FRAMEWORK_SCORE_MODEL,
    },
];

fn framework_convention_score(input: FrameworkConventionInput<'_>) -> u8 {
    if let Some(score) = bridge_framework_score(&input) {
        return score;
    }
    if let Some(score) = component_framework_score(&input) {
        return score;
    }
    if let Some(score) = php_route_framework_score(&input) {
        return score;
    }
    if !same_framework_language(&input) {
        return 0;
    }
    same_language_framework_score(&input)
}

fn bridge_framework_score(input: &FrameworkConventionInput<'_>) -> Option<u8> {
    let source_language = input.source.language.as_str();
    let target_language = input.target.language.as_str();
    let candidate = input.candidate;
    if javascript_family_name(source_language)
        && javascript_family_name(target_language)
        && candidate
            .qualified_name
            .contains("::turbo-module-spec-method::")
    {
        return Some(FRAMEWORK_SCORE_TURBO_MODULE);
    }
    if apple_framework_bridge_candidate(source_language, target_language, candidate) {
        return Some(FRAMEWORK_SCORE_APPLE_BRIDGE);
    }
    if javascript_family_name(source_language)
        && native_bridge_target_language(target_language)
        && native_javascript_bridge_candidate(candidate)
    {
        return Some(match target_language {
            "objc" => FRAMEWORK_SCORE_OBJC_BRIDGE,
            "swift" => FRAMEWORK_SCORE_SWIFT_BRIDGE,
            "java" | "kotlin" => FRAMEWORK_SCORE_JVM_BRIDGE,
            _ => 0,
        });
    }
    None
}

fn component_framework_score(input: &FrameworkConventionInput<'_>) -> Option<u8> {
    let source_language = input.source.language.as_str();
    let candidate_kind = input.candidate.kind;
    let target_path = input.target.path.as_str();
    if !matches!(source_language, "svelte" | "vue" | "astro" | "html") {
        return None;
    }
    if candidate_kind == SymbolKind::Component {
        if input.same_directory() {
            return Some(FRAMEWORK_SCORE_LOCAL_COMPONENT);
        }
        if directory_has_any(target_path, &["components", "component", "views", "pages"]) {
            return Some(FRAMEWORK_SCORE_CONVENTIONAL_COMPONENT);
        }
        return Some(FRAMEWORK_SCORE_COMPONENT_FALLBACK);
    }
    (source_language == "svelte"
        && matches!(candidate_kind, SymbolKind::Variable | SymbolKind::Constant)
        && directory_has_any(target_path, &["stores", "store"]))
    .then_some(FRAMEWORK_SCORE_STRONG)
}

fn php_route_framework_score(input: &FrameworkConventionInput<'_>) -> Option<u8> {
    let reference_name = input.reference_name;
    let target_language = input.target.language.as_str();
    let target_path = input.target.path.as_str();
    let candidate_kind = input.candidate.kind;
    if !php_route_source(input.source)
        || target_language != SourceLanguage::Php.as_str()
        || !directory_has_any(target_path, &["controller", "controllers"])
    {
        return None;
    }
    if candidate_kind == SymbolKind::Method && reference_name.contains("::") {
        return Some(FRAMEWORK_SCORE_ROUTE_METHOD);
    }
    (candidate_kind == SymbolKind::Class && reference_name.ends_with("Controller"))
        .then_some(FRAMEWORK_SCORE_CONVENTIONAL_COMPONENT)
}

fn same_framework_language(input: &FrameworkConventionInput<'_>) -> bool {
    input.source.language == input.target.language
        || javascript_family_name(&input.source.language)
            && javascript_family_name(&input.target.language)
}

fn same_language_framework_score(input: &FrameworkConventionInput<'_>) -> u8 {
    match input.source.language.as_str() {
        "typescript" | "tsx" | "javascript" | "jsx" => javascript_framework_score(input),
        "java" | "kotlin" | "scala" => framework_rule_score(input, JVM_FRAMEWORK_RULES),
        "ruby" => ruby_framework_score(input),
        "php" => framework_rule_score(input, PHP_FRAMEWORK_RULES),
        "csharp" => framework_rule_score(input, CSHARP_FRAMEWORK_RULES),
        "python" => framework_rule_score(input, PYTHON_FRAMEWORK_RULES),
        "swift" => framework_rule_score(input, SWIFT_FRAMEWORK_RULES),
        _ => framework_conventions::score(input),
    }
}

fn javascript_framework_score(input: &FrameworkConventionInput<'_>) -> u8 {
    let conventional_score = framework_rule_score(input, JAVASCRIPT_FRAMEWORK_RULES);
    if conventional_score != 0 {
        return conventional_score;
    }
    if !framework_pascal_case(input.reference_name)
        || !matches!(
            input.candidate.kind,
            SymbolKind::Component | SymbolKind::Function | SymbolKind::Class
        )
    {
        return 0;
    }
    if input.same_directory() {
        return FRAMEWORK_SCORE_EXACT_DIRECTORY;
    }
    if directory_has_any(
        &input.target.path,
        &["components", "views", "pages", "screens"],
    ) {
        return FRAMEWORK_SCORE_COMPONENT_FALLBACK;
    }
    FRAMEWORK_SCORE_PASCAL_FALLBACK
}

fn ruby_framework_score(input: &FrameworkConventionInput<'_>) -> u8 {
    if !framework_pascal_case(input.reference_name) || !framework_top_level_type(input.candidate) {
        return 0;
    }
    let directories = if input.reference_name.ends_with("Controller") {
        &["controllers"][..]
    } else if input.reference_name.ends_with("Helper") {
        &["helpers"][..]
    } else if input.reference_name.ends_with("Service") {
        &["services"][..]
    } else if input.reference_name.ends_with("Job") {
        &["jobs"][..]
    } else {
        &["models", "concerns"][..]
    };
    if directory_has_any(&input.target.path, directories) {
        FRAMEWORK_SCORE_EXACT_DIRECTORY
    } else {
        0
    }
}

fn framework_rule_score(input: &FrameworkConventionInput<'_>, rules: &[FrameworkRule]) -> u8 {
    rules
        .iter()
        .find(|rule| rule.matches(input))
        .map_or(0, |rule| rule.score)
}

fn framework_top_level_type(candidate: &ResolutionCandidate) -> bool {
    FRAMEWORK_TYPE_KINDS.contains(&candidate.kind)
}

fn framework_callable(candidate: &ResolutionCandidate) -> bool {
    matches!(candidate.kind, SymbolKind::Function | SymbolKind::Method)
}

fn framework_pascal_case(reference_name: &str) -> bool {
    reference_name
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_uppercase)
        && reference_name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn php_route_source(source: &ResolutionFileContext) -> bool {
    if !matches!(source.language.as_str(), "php" | "yaml") {
        return false;
    }
    let file_name = source.path.rsplit('/').next().unwrap_or(&source.path);
    source
        .path
        .split('/')
        .any(|segment| segment.eq_ignore_ascii_case("routes"))
        || file_name
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("routes"))
        || file_name.contains(".routing.")
}

fn php_route_candidate(input: PhpRouteCandidateInput<'_>) -> bool {
    let PhpRouteCandidateInput {
        source,
        target,
        source_file_id,
        candidate,
    } = input;
    php_route_source(source)
        && target.language == SourceLanguage::Php.as_str()
        && &candidate.file_id != source_file_id
        && candidate.visibility != Some(Visibility::Private)
        && matches!(candidate.kind, SymbolKind::Class | SymbolKind::Method)
        && directory_has_any(&target.path, &["controller", "controllers"])
}

fn php_route_resolution_fallbacks(reference_name: &str) -> [Option<String>; 2] {
    let Some((owner, member)) = reference_name.rsplit_once("::") else {
        return [None, None];
    };
    if member.ends_with("Controller") {
        return [Some(member.to_owned()), None];
    }
    let class = owner
        .rsplit(['\\', ':'])
        .find(|segment| !segment.is_empty());
    [
        class.map(|class| format!("{class}::{member}")),
        Some(member.to_owned()),
    ]
}

fn directory_has_any(path: &str, names: &[&str]) -> bool {
    path.split('/')
        .any(|segment| names.iter().any(|name| segment.eq_ignore_ascii_case(name)))
}

fn is_middleware_convention(name: &str) -> bool {
    javascript_framework_resolution::middleware_name(name)
}

fn project_scope_matches(
    source: &ResolutionFileContext,
    target: &ResolutionFileContext,
    candidate: &ResolutionCandidate,
) -> bool {
    if javascript_family_name(&source.language) && javascript_family_name(&target.language) {
        return true;
    }
    if c_include_family_name(&source.language) && c_include_family_name(&target.language) {
        return resolution_languages_compatible(&source.language, &target.language);
    }
    if source.language == target.language {
        return source.language != SourceLanguage::Go.as_str()
            || (source.directory == target.directory
                && source.package.is_some()
                && source.package == target.package);
    }
    framework_bridge_allowed(&source.language, &target.language, candidate)
}

fn framework_bridge_allowed(source: &str, target: &str, candidate: &ResolutionCandidate) -> bool {
    [
        javascript_component_bridge(source, target),
        salesforce_bridge(source, target),
        mybatis_bridge(source, target),
        apple_framework_bridge_candidate(source, target, candidate),
        config_constant_bridge(target, candidate),
        native_javascript_bridge(source, target, candidate),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn javascript_component_bridge(source: &str, target: &str) -> bool {
    matches!(source, "svelte" | "vue" | "astro" | "html") && javascript_family_name(target)
}

fn salesforce_bridge(source: &str, target: &str) -> bool {
    ((matches!(source, "aura" | "visualforce") || javascript_family_name(source))
        && target == "apex")
        || (source == "apex" && matches!(target, "aura" | "visualforce"))
}

fn mybatis_bridge(source: &str, target: &str) -> bool {
    (source == "xml" && matches!(target, "java" | "kotlin" | "scala"))
        || (matches!(source, "java" | "kotlin" | "scala") && target == "xml")
}

fn config_constant_bridge(target: &str, candidate: &ResolutionCandidate) -> bool {
    target == "properties" && candidate.kind == SymbolKind::Constant
}

fn native_javascript_bridge(source: &str, target: &str, candidate: &ResolutionCandidate) -> bool {
    javascript_family_name(source)
        && native_bridge_target_language(target)
        && native_javascript_bridge_candidate(candidate)
}

fn apple_framework_bridge_candidate(
    source: &str,
    target: &str,
    candidate: &ResolutionCandidate,
) -> bool {
    matches!((source, target), ("swift", "objc"))
        && candidate.qualified_name.contains("::objc-swift-method::")
        || matches!((source, target), ("objc", "swift"))
            && candidate.qualified_name.contains("::swift-objc-method::")
}

fn swift_base_names_for_objc_selector(selector: &str) -> [Option<String>; 3] {
    let raw = selector.rsplit('.').next().unwrap_or(selector);
    let without_trailing = raw.trim_end_matches(':');
    let first = without_trailing.split(':').next().unwrap_or_default();
    if first.is_empty() {
        return [None, None, None];
    }
    let literal = Some(first.to_owned());
    let mut reduced = None;
    if first.starts_with("initWith") {
        reduced = Some("init".to_owned());
    } else {
        for preposition in [
            "With", "For", "By", "In", "On", "At", "From", "To", "Of", "As",
        ] {
            if let Some(index) = first.find(preposition)
                && index > 0
                && first
                    .as_bytes()
                    .get(index + preposition.len())
                    .is_some_and(u8::is_ascii_uppercase)
                && first.as_bytes()[0].is_ascii_lowercase()
            {
                reduced = Some(first[..index].to_owned());
                break;
            }
        }
    }
    let property = (!without_trailing.contains(':')
        && raw.ends_with(':')
        && first.starts_with("set")
        && first.as_bytes().get(3).is_some_and(u8::is_ascii_uppercase))
    .then(|| {
        let value = &first[3..];
        let mut lowered = String::with_capacity(value.len());
        if let Some(byte) = value.as_bytes().first() {
            lowered.push(char::from(byte.to_ascii_lowercase()));
            lowered.push_str(&value[1..]);
        }
        lowered
    })
    .filter(|value| !value.is_empty());
    [literal, reduced, property]
}

fn native_bridge_target_language(language: &str) -> bool {
    matches!(language, "objc" | "swift" | "java" | "kotlin")
}

fn native_javascript_bridge_candidate(candidate: &ResolutionCandidate) -> bool {
    let qualified_name = candidate.qualified_name.as_str();
    match candidate.kind {
        SymbolKind::Method => {
            qualified_name.contains("::react-native-method::")
                || qualified_name.contains("::expo-module-method::")
        }
        SymbolKind::Resource => {
            qualified_name.contains("::react-native-module::")
                || qualified_name.contains("::expo-module::")
        }
        SymbolKind::Component => qualified_name.contains("::native-view-manager::"),
        SymbolKind::Property => qualified_name.contains("::native-view-prop::"),
        _ => false,
    }
}

fn javascript_family_name(language: &str) -> bool {
    matches!(language, "typescript" | "tsx" | "javascript" | "jsx")
}

fn c_include_family_name(language: &str) -> bool {
    matches!(language, "c" | "cpp" | "cuda" | "glsl" | "hlsl")
}

fn resolution_languages_compatible(source: &str, target: &str) -> bool {
    source == target
        || (jvm_resolution::language(source) && jvm_resolution::language(target))
        || (javascript_family_name(source) && javascript_family_name(target))
        || (matches!(source, "svelte" | "vue" | "astro" | "html") && javascript_family_name(target))
        || (javascript_family_name(source) && matches!(target, "svelte" | "vue" | "astro"))
        || (matches!(source, "c" | "cpp" | "cuda") && matches!(target, "c" | "cpp" | "cuda"))
        || (matches!(source, "wesl" | "wgsl") && matches!(target, "wesl" | "wgsl"))
}

fn select_candidate<'a, Candidates, Eligible, Cancel>(
    candidates: Candidates,
    mut eligible: Eligible,
    cancelled: &mut Cancel,
) -> Result<Option<&'a ResolutionCandidate>, StageItemFailure>
where
    Candidates: IntoIterator<Item = &'a ResolutionCandidate>,
    Eligible: FnMut(&ResolutionCandidate) -> bool,
    Cancel: FnMut() -> bool,
{
    let mut sole = None;
    let mut total = 0_usize;
    let mut implementation = None;
    let mut implementations = 0_usize;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !eligible(candidate) {
            continue;
        }
        total = total.saturating_add(1);
        sole = Some(candidate);
        if !candidate.implementation.declaration_only {
            implementations = implementations.saturating_add(1);
            implementation = Some(candidate);
        }
    }
    if implementations == 1 {
        Ok(implementation)
    } else if implementations == 0 && total == 1 {
        Ok(sole)
    } else {
        Ok(None)
    }
}

fn reference_input(input: ReferenceFactInput<'_>) -> ReferenceInput {
    let ReferenceFactInput {
        file_id,
        file_symbol_id,
        reference,
        resolution,
    } = input;
    let ReferenceResolution {
        target,
        unresolved_provenance,
    } = resolution;
    let (target_symbol_id, confidence, resolution_provenance) = match target {
        Some(target) => (
            Some(target.symbol_id),
            target.confidence,
            target.provenance.to_owned(),
        ),
        None => (
            None,
            UNRESOLVED_CONFIDENCE,
            unresolved_provenance.to_owned(),
        ),
    };
    ReferenceInput {
        file_id: file_id.clone(),
        owner_symbol_id: reference.owner.or_else(|| Some(file_symbol_id.clone())),
        target_symbol_id,
        reference_name: reference.name,
        reference_kind: reference.kind.as_str().to_owned(),
        start_byte: reference.span.start_byte(),
        end_byte: reference.span.end_byte(),
        confidence,
        resolution_provenance,
        site_count: 1,
        span_precision: ReferenceSpanPrecision::Exact,
    }
}

const REFERENCE_EDGE_KINDS: &[(ReferenceKind, EdgeKind)] = &[
    (ReferenceKind::Calls, EdgeKind::Calls),
    (ReferenceKind::Imports, EdgeKind::Imports),
    (ReferenceKind::References, EdgeKind::References),
    (ReferenceKind::Implements, EdgeKind::Implements),
    (ReferenceKind::Extends, EdgeKind::Extends),
    (ReferenceKind::Tests, EdgeKind::Tests),
    (ReferenceKind::Exports, EdgeKind::Exports),
    (ReferenceKind::TypeOf, EdgeKind::TypeOf),
    (ReferenceKind::Returns, EdgeKind::Returns),
    (ReferenceKind::Instantiates, EdgeKind::Instantiates),
    (ReferenceKind::Overrides, EdgeKind::Overrides),
    (ReferenceKind::Decorates, EdgeKind::Decorates),
    (ReferenceKind::FieldAccess, EdgeKind::FieldAccess),
    (ReferenceKind::DefUse, EdgeKind::DefUse),
];

fn reference_edge_kind(
    kind: ReferenceKind,
    target_kind: SymbolKind,
    source_kind: Option<SymbolKind>,
) -> Option<EdgeKind> {
    if kind == ReferenceKind::Inherits {
        qualified_member_resolution::inheritance_edge_kind(target_kind, source_kind)
    } else {
        REFERENCE_EDGE_KINDS
            .iter()
            .find_map(|(reference, edge)| (*reference == kind).then_some(*edge))
    }
}

fn native_document_id(kind: &str, identity: &str) -> DocumentId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOCUMENT_ID_DOMAIN);
    hash_text(&mut hasher, kind);
    hash_text(&mut hasher, identity);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; DOCUMENT_UUID_BYTES];
    bytes.copy_from_slice(&digest.as_bytes()[..DOCUMENT_UUID_BYTES]);
    DocumentId::from_uuid_v8(bytes)
}

fn native_file_symbol_input(input: &FileRecordInput<'_>) -> Result<SymbolInput, StageItemFailure> {
    Ok(SymbolInput {
        symbol_id: input.file_symbol_id.clone(),
        file_id: input.identity.file_id.clone(),
        symbol_kind: SymbolKind::File.as_str().to_owned(),
        qualified_name: file_symbol_qualified_name(input.identity)?,
        signature: String::new(),
        start_byte: 0,
        end_byte: input.file.byte_size,
        start_line: 1,
        end_line: input.line_count,
        structural_digest: input.file.content_hash.clone(),
        visibility: None,
        export: SymbolExportFlags::default(),
        execution: SymbolExecutionFlags::default(),
        declaration_only: false,
        betweenness_ppb: None,
        pagerank_ppb: None,
    })
}

fn native_file_symbol_id(file_id: &FileId) -> SymbolId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(FILE_SYMBOL_ID_DOMAIN);
    hash_text(&mut hasher, file_id.as_str());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; DOCUMENT_UUID_BYTES];
    bytes.copy_from_slice(&digest.as_bytes()[..DOCUMENT_UUID_BYTES]);
    SymbolId::from_uuid_v8(bytes)
}

fn file_symbol_qualified_name(identity: &FileDocumentIdentity) -> Result<String, StageItemFailure> {
    if identity.path.len() <= MAX_SYMBOL_QUALIFIED_NAME_BYTES {
        return try_clone_text(&identity.path);
    }
    let length = FILE_SYMBOL_FALLBACK_PREFIX
        .len()
        .checked_add(identity.file_id.as_str().len())
        .ok_or(StageItemFailure)?;
    let mut name = String::new();
    name.try_reserve(length).map_err(|_| StageItemFailure)?;
    name.push_str(FILE_SYMBOL_FALLBACK_PREFIX);
    name.push_str(identity.file_id.as_str());
    Ok(name)
}

fn hash_text(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&usize_to_u64(value.len()).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn document_kind_for_path(path: &str, ordinary: DocumentKind) -> DocumentKind {
    if is_test_source_path(path) {
        DocumentKind::Test
    } else {
        ordinary
    }
}

fn modeled_file_input_bytes(file: &FileInput) -> u64 {
    usize_to_u64(size_of::<FileInput>())
        .saturating_add(usize_to_u64(file.file_id.as_str().len()))
        .saturating_add(usize_to_u64(file.normalized_path.capacity()))
        .saturating_add(usize_to_u64(file.language.capacity()))
        .saturating_add(usize_to_u64(file.content_hash.as_str().len()))
}

fn modeled_symbol_input_bytes(symbol: &SymbolInput) -> u64 {
    usize_to_u64(size_of::<SymbolInput>())
        .saturating_add(usize_to_u64(symbol.symbol_id.as_str().len()))
        .saturating_add(usize_to_u64(symbol.file_id.as_str().len()))
        .saturating_add(usize_to_u64(symbol.symbol_kind.capacity()))
        .saturating_add(usize_to_u64(symbol.qualified_name.capacity()))
        .saturating_add(usize_to_u64(symbol.signature.capacity()))
        .saturating_add(usize_to_u64(symbol.structural_digest.as_str().len()))
}

fn anticipated_file_document_bytes(file: &FileInput) -> u64 {
    usize_to_u64(size_of::<SearchDocumentInput>())
        .saturating_add(UUID_TEXT_BYTES.saturating_mul(2))
        .saturating_add(usize_to_u64(file.normalized_path.len()).saturating_mul(2))
        .saturating_add(usize_to_u64(file.language.len()))
        .saturating_add(DOCUMENT_METADATA_FIXED_ALLOWANCE)
}

fn anticipated_file_symbol_bytes(file: &FileInput) -> u64 {
    usize_to_u64(size_of::<SymbolInput>())
        .saturating_add(UUID_TEXT_BYTES.saturating_mul(2))
        .saturating_add(usize_to_u64(SymbolKind::File.as_str().len()))
        .saturating_add(usize_to_u64(file.normalized_path.len()))
        .saturating_add(usize_to_u64(file.content_hash.as_str().len()))
}

fn anticipated_document_bytes(
    symbol: &NativeSymbolFacts,
    path_bytes: u64,
    language_bytes: u64,
) -> u64 {
    let base_code_bytes = if symbol.input.signature.is_empty() {
        usize_to_u64(symbol.input.qualified_name.len())
    } else {
        usize_to_u64(symbol.input.signature.len())
    };
    let code_bytes = base_code_bytes
        .saturating_add(usize_to_u64(symbol.body_search_text.len()))
        .saturating_add(u64::from(!symbol.body_search_text.is_empty()));
    usize_to_u64(size_of::<SearchDocumentInput>())
        .saturating_add(UUID_TEXT_BYTES.saturating_mul(3))
        .saturating_add(path_bytes)
        .saturating_add(language_bytes)
        .saturating_add(usize_to_u64(symbol.input.qualified_name.len()))
        .saturating_add(code_bytes)
        .saturating_add(
            symbol
                .docstring
                .as_ref()
                .map_or(0, |docstring| usize_to_u64(docstring.len())),
        )
        .saturating_add(usize_to_u64(symbol.name.capacity()))
        .saturating_add(usize_to_u64(symbol.clone_shape_digest.as_str().len()))
        .saturating_add(DOCUMENT_METADATA_FIXED_ALLOWANCE)
}

fn symbol_document_code(symbol: &NativeSymbolFacts) -> Result<String, StageItemFailure> {
    let base = if symbol.input.signature.is_empty() {
        &symbol.input.qualified_name
    } else {
        &symbol.input.signature
    };
    let separator = usize::from(!base.is_empty() && !symbol.body_search_text.is_empty());
    let capacity = base
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(symbol.body_search_text.len()))
        .ok_or(StageItemFailure)?;
    let mut code = String::new();
    code.try_reserve(capacity).map_err(|_| StageItemFailure)?;
    code.push_str(base);
    if separator != 0 {
        code.push(' ');
    }
    code.push_str(&symbol.body_search_text);
    Ok(code)
}

fn try_clone_text(value: &str) -> Result<String, StageItemFailure> {
    let mut cloned = String::new();
    cloned
        .try_reserve(value.len())
        .map_err(|_| StageItemFailure)?;
    cloned.push_str(value);
    Ok(cloned)
}

fn resolve_reservation(maximum_generation_bytes: u64) -> Result<u64, NativePipelineError> {
    maximum_generation_bytes
        .checked_mul(RESOLVE_WORKING_MULTIPLIER)
        .ok_or(NativePipelineError::Incomplete {
            stage: PipelineStage::Resolve,
        })
}

fn generation_validation_limits(
    maximum_generation_bytes: u64,
    stage: PipelineStage,
) -> Result<GenerationValidationLimits, NativePipelineError> {
    let maximum_working_bytes = maximum_generation_bytes
        .checked_mul(VALIDATION_WORKING_MULTIPLIER)
        .ok_or(NativePipelineError::Incomplete { stage })?;
    GenerationValidationLimits::new(maximum_generation_bytes, maximum_working_bytes)
        .map_err(|_| NativePipelineError::Incomplete { stage })
}

fn vector_capacity_bytes<T>(values: &Vec<T>) -> u64 {
    usize_to_u64(values.capacity()).saturating_mul(usize_to_u64(size_of::<T>()))
}

fn validate_retained_limit(
    value: u64,
    field: &'static str,
) -> Result<(), NativePipelineConfigError> {
    if value == 0 || value > MAX_PIPELINE_RETAINED_BYTES {
        Err(NativePipelineConfigError::invalid(field))
    } else {
        Ok(())
    }
}

fn validate_capacity(
    capacity: StageCapacity,
    field: &'static str,
) -> Result<(), NativePipelineConfigError> {
    if capacity.workers() == 0
        || capacity
            .workers()
            .checked_add(capacity.queued_items())
            .is_none()
    {
        Err(NativePipelineConfigError::invalid(field))
    } else {
        Ok(())
    }
}

fn require_multithread_runtime() -> Result<(), NativePipelineError> {
    let runtime = Handle::try_current().map_err(|_| NativePipelineError::Runtime)?;
    if matches!(runtime.runtime_flavor(), RuntimeFlavor::MultiThread) {
        Ok(())
    } else {
        Err(NativePipelineError::Runtime)
    }
}

fn planned_item_deadline(item_timeout: Duration, stage_deadline: Instant) -> Instant {
    Instant::now()
        .checked_add(item_timeout)
        .unwrap_or(stage_deadline)
        .min(stage_deadline)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    mod frameworks;
    mod generic_digest_proof;
    mod generic_repair;
    mod generic_resolution;
    mod javascript_members;
    mod javascript_modules;
    mod javascript_parity;
    mod jvm_resolution;
    mod modules;
    mod pascal_units;
    mod php_namespaces;
    mod polyglot_parity;
    mod python_imports;
    mod qualified_types;
    mod receiver_types;
    mod repr;
    mod rust_module_paths;
    mod rust_receivers;
    mod rust_review_regressions;
    mod rust_use_bindings;
    mod rust_use_regressions;
    mod script_modules;
    mod unqual_digests;
    mod unqual_repair;
    mod unqual_resolution;
    mod v1_resolution_oracle;

    use std::assert_matches;
    use std::{cell::Cell, collections::BTreeSet, fmt::Write as _, fs, time::Duration};

    use cartograph_scip::{
        CartographScipEdge, SYMBOL_ROLE_DEFINITION, ScipDocument, ScipIndex, ScipOccurrence,
        ScipSymbolInformation, encode_scip_index,
    };
    use tempfile::tempdir;

    use super::*;
    use crate::stage::test_stage_runner;

    const TEST_FILES: usize = 128;
    const TEST_PATH_BYTES: u64 = 2 * 1024 * 1024;
    const TEST_SOURCE_BYTES: usize = 1024 * 1024;
    const TEST_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
    const TEST_GENERATION_BYTES: u64 = 32 * 1024 * 1024;
    const MEGA_TEST_GENERATION_BYTES: u64 = 64 * 1024 * 1024;
    const TEST_SCOPE_BYTES: u64 = 128 * 1024 * 1024;
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const CLEANUP_GRACE: Duration = Duration::from_millis(500);
    const SERIAL_WORKERS: usize = 1;
    const PARALLEL_WORKERS: usize = 4;
    const DRIFT_SCOPE_TASKS: usize = 8;
    const EXPECTED_SOURCE_FILES: u64 = 3;
    const EXPECTED_MINIMUM_SYMBOLS: u64 = 4;
    const EXPECTED_MINIMUM_RESOLVED_REFERENCES: u64 = 3;
    const REJECTING_GENERATION_BYTES: u64 = 1;
    const INITIALIZER_SECRET: &str = "sk-cartograph-never-index-this";
    const SCALAR_DEFAULT_SECRET: &str = "sk-cartograph-scalar-default";
    const DESTRUCTURED_DEFAULT_SECRET: &str = "sk-cartograph-destructured-default";
    const ARROW_DEFAULT_SECRET: &str = "sk-cartograph-arrow-default";
    const METHOD_DEFAULT_SECRET: &str = "sk-cartograph-method-default";
    const COMPONENT_DEFAULT_SECRET: &str = "sk-cartograph-component-default";
    const SECRET_LITERAL_COUNT: usize = 6;
    const LONG_PATH_COMPONENT_BYTES: usize = 190;
    const LONG_PATH_COMPONENT_COUNT: usize = 11;
    const MINIMUM_LONG_PATH_BYTES: usize = 2_048;
    const MANY_SYMBOL_COUNT: usize = 256;
    const CANCELLATION_SYMBOL_COUNT: usize = 128;
    const CANCEL_AFTER_POLLS: u64 = 16;
    const INNER_CANCELLATION_CANDIDATE_COUNT: u8 = 64;
    const INNER_CANCEL_AFTER_POLLS: u64 = 8;
    const TEST_FILE_ID_BYTE: u8 = 0x21;
    const TEST_SYMBOL_ID_BYTE: u8 = 0x42;
    const CONFIDENCE_TOLERANCE: f32 = 1.0e-6;

    fn confidence_matches(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() <= CONFIDENCE_TOLERANCE
    }

    fn assert_confidence(actual: f32, expected: f32) {
        assert!(
            confidence_matches(actual, expected),
            "confidence mismatch: expected {expected}, got {actual}"
        );
    }

    fn append_fixture_text(output: &mut String, arguments: std::fmt::Arguments<'_>) {
        output
            .write_fmt(arguments)
            .unwrap_or_else(|error| panic!("could not construct source fixture: {error}"));
    }

    fn test_source_root() -> SourceRoot {
        SourceRoot::open(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
            .unwrap_or_else(|error| panic!("test source root failed: {error}"))
    }

    const FULL_TEST_EVIDENCE: NativeEvidencePolicy = NativeEvidencePolicy::FULL;
    const STRUCTURAL_TEST_EVIDENCE: NativeEvidencePolicy = NativeEvidencePolicy::STRUCTURAL;
    // V22 changes the digest domain; facts unchanged in these fixtures.
    const PARSER_ONLY_FILE_COUNT: usize = 6;
    const EXPECTED_PARSER_ONLY_DIGEST: &str =
        "f483007b8c7865ce060e125c6c1192d44245d21f9d8c29ede69cf80933d8da4b";
    const EXPECTED_PARSER_ONLY_PROJECTION: (usize, usize, usize, usize, usize) = (6, 6, 0, 0, 6);
    const ADMITTED_FAMILY_FILE_COUNT: usize = 14;
    const EXPECTED_ADMITTED_FAMILY_DIGEST: &str =
        "f2ab906f518d9e51853532b887e8ffe33049270a5c049fd5b02cbafdcc9da8d4";
    const EXPECTED_ADMITTED_FAMILY_PROJECTION: (usize, usize, usize, usize, usize) =
        (14, 33, 19, 6, 33);
    const GENERIC_FAMILY_FILE_COUNT: usize = 28;
    // Pascal now has a dedicated family: fixture.pas extracts its full structure
    // (2 -> 9 symbols, the projection's +7) and its runtime calls (`SetLength`,
    // `Length`, `High`) stay unresolved as external references. The SQL
    // function body's `INSERT INTO box` now references and resolves to table
    // `box`, as v1 recorded (the projection's +1 reference and +1 edge).
    // ABAP declares classes and methods at their IMPLEMENTATION blocks, so
    // fixture.abap's class and method move there (same counts, new spans,
    // and the method now owns its body).
    // Dart fixture.dart:6 now declares Box::Box, adding exactly its method,
    // Box -> Box::Box containment edge, and symbol search document. Removing
    // those three facts restored the previous dcf6c35b... digest under V21.
    // Box::Box is concrete: declaration_only is false in the symbol and its
    // document metadata. Restoring only those two booleans to true restored
    // the intermediate 36d77193... digest under V21; all counts stay unchanged.
    // Resolution adds CounterView::build -> CounterView::increment (ArkTS)
    // and targets for its two existing references. generic_digest_proof removes
    // that one Calls edge and restores those references to reproduce the
    // previous fact set under V22 (8fe8df25...); the extraction facts stay fixed.
    const EXPECTED_GENERIC_FAMILY_DIGEST: &str =
        "a4e55aab2f0dbbaa33046f1a9975810b75c6cee9ea831482cdda5b958a437e2b";
    const EXPECTED_GENERIC_FAMILY_PROJECTION: (usize, usize, usize, usize, usize) =
        (28, 260, 284, 135, 260);
    const CUSTOM_FAMILY_FILE_COUNT: usize = 13;
    // v1 parity: Anubis handlers carry their `:<line>` suffix, an Osiris block
    // spans from its `IF` line and is named by its head line, and an LSX
    // resource starts at its `<node>` tag (same projection, new identities).
    const EXPECTED_CUSTOM_FAMILY_DIGEST: &str =
        "6ef2ddf7ec04c15deb9fbad64b9f080695c8048396e465c2115893ec03d6a64d";
    // A Liquid `{% render %}` partner is a Component as well as an Import
    // (+1 symbol and its containment).
    const EXPECTED_CUSTOM_FAMILY_PROJECTION: (usize, usize, usize, usize, usize) =
        (13, 50, 45, 32, 50);
    const CUSTOM_FAMILY_FIXTURES: [(&str, &str, SourceLanguage); CUSTOM_FAMILY_FILE_COUNT] = [
        (
            "force-app/main/default/aura/OrderPanel/OrderPanel.cmp",
            "<aura:component controller=\"OrderController\"><aura:attribute name=\"orderId\" type=\"Id\"/><c:orderCard onclick=\"{!c.loadOrder}\"/></aura:component>\n",
            SourceLanguage::Aura,
        ),
        (
            "custom/order.ann",
            "game.states.OrderState = State {\n nodes.LoadOrder = Action {\n OnEnter = function()\n StartOrder()\n end\n}\n",
            SourceLanguage::Bg3Anubis,
        ),
        (
            "Mods/Orders/Public/Data/order.lsx",
            "<save><region id=\"Orders\"><node id=\"OrderDefinition\"><attribute id=\"Name\" value=\"OrderResourceBeacon\"/><attribute id=\"ParentTemplateId\" value=\"OrderParent_123\"/></node></region></save>\n",
            SourceLanguage::Bg3Resource,
        ),
        (
            "Game/Stats/Generated/Data/orders.txt",
            "new entry \"OrderStatsBeacon\"\nusing \"BaseOrderStats\"\n",
            SourceLanguage::Bg3Stats,
        ),
        (
            "sections/order-panel.liquid",
            "{% assign order_total = cart.total %}\n{% render 'order-card' %}\n{{ format_order(order_total) }}\n",
            SourceLanguage::Liquid,
        ),
        (
            "Story/RawFiles/Goals/OrderGoal.txt",
            "INITSECTION\nsyscall StartOrder((GUIDSTRING)_Order)\nKBSECTION\nIF\nDB_OrderReady(_Order)\nTHEN\nStartOrder(_Order);\n",
            SourceLanguage::Osiris,
        ),
        (
            "config/application.properties",
            "orders.cache.ttl=${orders.default.ttl}\n",
            SourceLanguage::Properties,
        ),
        (
            "scripts/order-policy.rhai",
            include_str!("../../../docs/test-beds/rhai/fixture.rhai"),
            SourceLanguage::Rhai,
        ),
        (
            "components/OrderPanel.svelte",
            "<script lang=\"ts\">\nexport function loadOrder() { fetchOrder(); }\n</script>\n<OrderCard on:click=\"loadOrder()\" />\n",
            SourceLanguage::Svelte,
        ),
        (
            "legacy/OrderModule.bas",
            "Attribute VB_Name = \"OrderModule\"\nPublic Sub LoadOrder()\n FetchOrder (1)\nEnd Sub\n",
            SourceLanguage::Vb6,
        ),
        (
            "force-app/main/default/pages/Orders.page",
            "<apex:page controller=\"OrderController\" action=\"{!loadOrders}\"><c:orderTable/></apex:page>\n",
            SourceLanguage::Visualforce,
        ),
        (
            "components/OrderPanel.vue",
            "<script setup lang=\"ts\">\nexport function loadOrder() { fetchOrder(); }\n</script>\n<template><OrderCard @click=\"loadOrder()\" /></template>\n",
            SourceLanguage::Vue,
        ),
        (
            "mappers/OrderMapper.xml",
            "<mapper namespace=\"com.example.OrderMapper\"><sql id=\"orderColumns\">id</sql><select id=\"findOrder\">SELECT <include refid=\"orderColumns\"/> WHERE id=#{orderId}</select></mapper>\n",
            SourceLanguage::Xml,
        ),
    ];
    const GENERIC_FAMILY_FIXTURES: [(&str, &str, SourceLanguage); GENERIC_FAMILY_FILE_COUNT] = [
        (
            "generic/fixture.abap",
            include_str!("../../../docs/test-beds/abap/fixture.abap"),
            SourceLanguage::Abap,
        ),
        (
            "generic/fixture.cls",
            include_str!("../../../docs/test-beds/apex/fixture.cls"),
            SourceLanguage::Apex,
        ),
        (
            "generic/fixture.ets",
            include_str!("../../../docs/test-beds/arkts/fixture.ets"),
            SourceLanguage::ArkTs,
        ),
        (
            "generic/fixture.astro",
            include_str!("../../../docs/test-beds/astro/fixture.astro"),
            SourceLanguage::Astro,
        ),
        (
            "generic/fixture.clj",
            "(ns beacon.core)\n(defn clojureBeacon [] 1)\n",
            SourceLanguage::Clojure,
        ),
        (
            "generic/fixture.lisp",
            "(defpackage :beacon)\n(in-package :beacon)\n(defun lisp-beacon () 1)\n",
            SourceLanguage::CommonLisp,
        ),
        (
            "generic/fixture.dart",
            include_str!("../../../docs/test-beds/dart/fixture.dart"),
            SourceLanguage::Dart,
        ),
        (
            "generic/fixture.fs",
            include_str!("../../../docs/test-beds/fsharp/fixture.fs"),
            SourceLanguage::FSharp,
        ),
        (
            "generic/fixture.graphql",
            include_str!("../../../docs/test-beds/graphql/fixture.graphql"),
            SourceLanguage::GraphQl,
        ),
        (
            "generic/fixture.tf",
            include_str!("../../../docs/test-beds/hcl/fixture.tf"),
            SourceLanguage::Hcl,
        ),
        (
            "generic/fixture.html",
            include_str!("../../../docs/test-beds/html/fixture.html"),
            SourceLanguage::Html,
        ),
        (
            "generic/fixture.khn",
            "function khnBeacon() return 1 end\n",
            SourceLanguage::Khn,
        ),
        (
            "generic/fixture.lean",
            include_str!("../../../docs/test-beds/lean/fixture.lean"),
            SourceLanguage::Lean,
        ),
        (
            "generic/fixture.lua",
            include_str!("../../../docs/test-beds/lua/fixture.lua"),
            SourceLanguage::Lua,
        ),
        (
            "generic/fixture.luau",
            include_str!("../../../docs/test-beds/luau/fixture.luau"),
            SourceLanguage::Luau,
        ),
        (
            "generic/fixture.nix",
            "{ nixBeacon = 1; }\n",
            SourceLanguage::Nix,
        ),
        (
            "generic/fixture.m",
            include_str!("../../../docs/test-beds/objc/fixture.m"),
            SourceLanguage::ObjectiveC,
        ),
        (
            "generic/fixture.pas",
            include_str!("../../../docs/test-beds/pascal/fixture.pas"),
            SourceLanguage::Pascal,
        ),
        (
            "generic/fixture.php",
            include_str!("../../../docs/test-beds/php/fixture.php"),
            SourceLanguage::Php,
        ),
        (
            "generic/fixture.prisma",
            include_str!("../../../docs/test-beds/prisma/fixture.prisma"),
            SourceLanguage::Prisma,
        ),
        (
            "generic/fixture.r",
            include_str!("../../../docs/test-beds/r/fixture.r"),
            SourceLanguage::R,
        ),
        (
            "generic/fixture.res",
            include_str!("../../../docs/test-beds/rescript/fixture.res"),
            SourceLanguage::ReScript,
        ),
        (
            "generic/fixture.rb",
            include_str!("../../../docs/test-beds/ruby/fixture.rb"),
            SourceLanguage::Ruby,
        ),
        (
            "generic/fixture.sol",
            include_str!("../../../docs/test-beds/solidity/fixture.sol"),
            SourceLanguage::Solidity,
        ),
        (
            "generic/fixture.sql",
            include_str!("../../../docs/test-beds/sql/fixture.sql"),
            SourceLanguage::Sql,
        ),
        (
            "generic/fixture.swift",
            include_str!("../../../docs/test-beds/swift/fixture.swift"),
            SourceLanguage::Swift,
        ),
        (
            "generic/fixture.vb",
            include_str!("../../../docs/test-beds/vbnet/fixture.vb"),
            SourceLanguage::VbNet,
        ),
        (
            "generic/fixture.yaml",
            include_str!("../../../docs/test-beds/yaml/fixture.yaml"),
            SourceLanguage::Yaml,
        ),
    ];
    const ADMITTED_FAMILY_FIXTURES: [(&str, &str, SourceLanguage); ADMITTED_FAMILY_FILE_COUNT] = [
        (
            "native/cbeacon.c",
            "int cbeacon(void) { return 1; }\n",
            SourceLanguage::C,
        ),
        (
            "native/cppbeacon.cpp",
            "int cppbeacon() { return 1; }\n",
            SourceLanguage::Cpp,
        ),
        (
            "native/cudabeacon.cu",
            "__global__ void cudabeacon() {}\n",
            SourceLanguage::Cuda,
        ),
        (
            "native/glslbeacon.glsl",
            "void glslbeacon() {}\n",
            SourceLanguage::Glsl,
        ),
        (
            "native/hlslbeacon.hlsl",
            "float4 hlslbeacon() : SV_Target { return float4(1, 1, 1, 1); }\n",
            SourceLanguage::Hlsl,
        ),
        (
            "native/bashbeacon.sh",
            "bashbeacon() { echo ok; }\n",
            SourceLanguage::Bash,
        ),
        (
            "native/fishbeacon.fish",
            "function fishbeacon\n  echo ok\nend\n",
            SourceLanguage::Fish,
        ),
        (
            "native/powershellbeacon.ps1",
            "function PowershellBeacon { Write-Output ok }\n",
            SourceLanguage::PowerShell,
        ),
        (
            "native/zshbeacon.zsh",
            "zshbeacon() { print ok; }\n",
            SourceLanguage::Zsh,
        ),
        (
            "native/JavaBeacon.java",
            "public class JavaBeacon { public void runBeacon() {} }\n",
            SourceLanguage::Java,
        ),
        (
            "native/CsharpBeacon.cs",
            "public class CsharpBeacon { public void RunBeacon() {} }\n",
            SourceLanguage::CSharp,
        ),
        (
            "native/KotlinBeacon.kt",
            "class KotlinBeacon { fun runBeacon() {} }\n",
            SourceLanguage::Kotlin,
        ),
        (
            "native/ScalaBeacon.scala",
            "class ScalaBeacon { def runBeacon(): Unit = () }\n",
            SourceLanguage::Scala,
        ),
        (
            "native/GroovyBeacon.groovy",
            "class GroovyBeacon { void runBeacon() {} }\n",
            SourceLanguage::Groovy,
        ),
    ];
    const C_RESOLVER_FIXTURES: [(&str, &str); 10] = [
        (
            "include/api.hpp",
            "namespace api {\nclass Worker { public: void run(); static void ping(); private: void private_run(); };\nWorker *make_worker();\nint shared_value();\n}\n",
        ),
        (
            "include/other.hpp",
            "namespace other {\nclass Worker { public: static void ping(); private: void run(); };\nclass Solitary { public: void orphan(); };\n}\n",
        ),
        (
            "src/api.cpp",
            "#include \"../include/api.hpp\"\nvoid api::Worker::run() { helper(); }\nvoid api::Worker::ping() {}\nvoid api::Worker::private_run() {}\napi::Worker *api::make_worker() { return new api::Worker(); }\nint api::shared_value() { return 1; }\n",
        ),
        (
            "src/other.cpp",
            "#include \"../include/other.hpp\"\nvoid other::Worker::ping() {}\n",
        ),
        (
            "src/main.cpp",
            "#include \"../include/api.hpp\"\n#include \"../include/other.hpp\"\n#include <vector>\nint use_api() {\n  auto *worker = api::make_worker();\n  worker->run();\n  api::Worker::ping();\n  ping();\n  api::Worker::private_run();\n  auto *direct = new api::Worker();\n  return api::shared_value();\n}\n",
        ),
        ("src/unrelated.cpp", "void run() {}\n"),
        ("src/ambiguous_a.cpp", "int duplicate() { return 1; }\n"),
        ("src/ambiguous_b.cpp", "int duplicate() { return 2; }\n"),
        (
            "src/negative.cpp",
            "#include <missing_system.h>\nint unresolved_use() { orphan(); return duplicate() + missing_system(); }\n",
        ),
        (
            "src/missing.cpp",
            "#include \"../include/missing.hpp\"\nint missing_header_use() { return 0; }\n",
        ),
    ];
    const CSHARP_RESOLVER_FIXTURES: [(&str, &str); 2] = [
        (
            "src/Bases.cs",
            "class IPhone {}\ninterface Disposable {}\nclass Device : IPhone, Disposable {}\nclass MissingDevice : MissingBase {}\n",
        ),
        ("src/Unrelated.cs", "class Unrelated {}\n"),
    ];
    const PARSER_ONLY_FIXTURES: [(&str, &str, SourceLanguage); PARSER_ONLY_FILE_COUNT] = [
        (
            "styles/site.css",
            "body { color: red; }",
            SourceLanguage::Css,
        ),
        (
            "views/user.erb",
            "<div><%= user.name %></div>",
            SourceLanguage::EmbeddedTemplate,
        ),
        (
            "docs/api.jsdoc",
            "/** Adds one. */\n",
            SourceLanguage::JsDoc,
        ),
        (
            "config/app.json",
            r#"{"enabled":true}"#,
            SourceLanguage::Json,
        ),
        (
            "notebooks/demo.ipynb",
            r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#,
            SourceLanguage::Jupyter,
        ),
        (
            "patterns/email.regex",
            r"[a-z]+@[a-z]+\.[a-z]+",
            SourceLanguage::Regex,
        ),
    ];
    const CONTENT_CLASSIFIER_NEGATIVES: [(&str, &str); 4] = [
        ("unrelated/widget.cmp", "<widget />\n"),
        ("unrelated/widget.page", "<html />\n"),
        ("unrelated/widget.component", "<component />\n"),
        ("notes/plain.md", "plain markdown without front matter\n"),
    ];

    struct ModuleResolverFacts<'a> {
        facts: &'a CanonicalGenerationFacts,
    }

    struct ExpectedResolvedReference {
        caller_path: &'static str,
        caller_name: &'static str,
        reference_name: &'static str,
        target_path: &'static str,
        target_name: &'static str,
        provenance: &'static str,
    }

    const POLYGLOT_RESOLVED_REFERENCES: [ExpectedResolvedReference; 24] = [
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_use",
            reference_name: "nested::nested_helper",
            target_path: "nested/mod.rs",
            target_name: "nested_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_dynamic_use",
            reference_name: "worker.unique_finish",
            target_path: "rust_helper.rs",
            target_name: "LateWorker::unique_finish",
            provenance: DYNAMIC_DISPATCH_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_callback_use",
            reference_name: "enabled",
            target_path: "lib.rs",
            target_name: "rust_callback_use::enabled",
            provenance: EXACT_LEXICAL_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_use",
            reference_name: "rust_helper::rust_helper",
            target_path: "rust_helper.rs",
            target_name: "rust_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_use",
            reference_name: "rust_helper::LateWorker::orphan_method",
            target_path: "rust_helper.rs",
            target_name: "LateWorker::orphan_method",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_qualified_use",
            reference_name: "crate::rust_helper::LateWorker::orphan_method",
            target_path: "rust_helper.rs",
            target_name: "LateWorker::orphan_method",
            provenance: RUST_QUALIFIED_PATH_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_root_reexport_use",
            reference_name: "crate::LateWorker::orphan_method",
            target_path: "rust_helper.rs",
            target_name: "LateWorker::orphan_method",
            provenance: RUST_QUALIFIED_PATH_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "lib.rs",
            caller_name: "rust_macro_use",
            reference_name: "rust_helper::rust_helper",
            target_path: "rust_helper.rs",
            target_name: "rust_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "grouped_use.rs",
            caller_name: "grouped_rust_use",
            reference_name: "nested::nested_helper",
            target_path: "nested/mod.rs",
            target_name: "nested_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "grouped_use.rs",
            caller_name: "grouped_rust_use",
            reference_name: "helper_alias::rust_helper",
            target_path: "rust_helper.rs",
            target_name: "rust_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "grouped_use.rs",
            caller_name: "grouped_rust_use",
            reference_name: "nested_helper",
            target_path: "nested/mod.rs",
            target_name: "nested_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "grouped_use.rs",
            caller_name: "rust_function_item_use",
            reference_name: "LateWorker::orphan_method",
            target_path: "rust_helper.rs",
            target_name: "LateWorker::orphan_method",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "walk.rs",
            caller_name: "visit_usage",
            reference_name: "references::capture_usage",
            target_path: "walk/references.rs",
            target_name: "capture_usage",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "walk.rs",
            caller_name: "visit_usage",
            reference_name: "self::references::capture_usage",
            target_path: "walk/references.rs",
            target_name: "capture_usage",
            provenance: RUST_QUALIFIED_PATH_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "walk/references.rs",
            caller_name: "capture_usage",
            reference_name: "parent_helper",
            target_path: "walk.rs",
            target_name: "parent_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "py_consumer.py",
            caller_name: "python_use",
            reference_name: "python_helper",
            target_path: "py_helpers.py",
            target_name: "python_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "py_namespace_consumer.py",
            caller_name: "python_namespace_use",
            reference_name: "helpers.python_helper",
            target_path: "py_helpers.py",
            target_name: "python_helper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "go_consumer.go",
            caller_name: "GoUse",
            reference_name: "GoHelper",
            target_path: "go_helpers.go",
            target_name: "GoHelper",
            provenance: EXACT_PROJECT_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "use_barrel.ts",
            caller_name: "consume",
            reference_name: "renamed",
            target_path: "core.ts",
            target_name: "core",
            provenance: "native-reexport-alias",
        },
        ExpectedResolvedReference {
            caller_path: "use_local_alias.ts",
            caller_name: "consumeLocalAlias",
            reference_name: "publicInternal",
            target_path: "local_alias.ts",
            target_name: "publicInternal",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "use_default_local.ts",
            caller_name: "useDefaultLocal",
            reference_name: "selected",
            target_path: "default_local.ts",
            target_name: "defaultLocal",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "cjs_consumer.cjs",
            caller_name: "cjsUse",
            reference_name: "helper.cjsHelper",
            target_path: "cjs_helper.js",
            target_name: "cjsHelper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "cjs_destructured.cjs",
            caller_name: "cjsSelectedUse",
            reference_name: "selectedHelper",
            target_path: "cjs_helper.js",
            target_name: "cjsHelper",
            provenance: IMPORT_BINDING_PROVENANCE,
        },
        ExpectedResolvedReference {
            caller_path: "local_require.cjs",
            caller_name: "localRequireUse",
            reference_name: "require",
            target_path: "local_require.cjs",
            target_name: "require",
            provenance: EXACT_SAME_FILE_PROVENANCE,
        },
    ];

    const POLYGLOT_DIRECTORIES: [&str; 6] =
        ["other", "nested", "conflict", "index_only", "fake", "walk"];

    const POLYGLOT_FIXTURES: [(&str, &str); 34] = [
        (
            "rust_helper.rs",
            "impl LateWorker { pub fn orphan_method() -> usize { 8 } pub fn unique_finish(&self) -> usize { 10 } pub fn ambiguous_finish(&self) -> usize { 11 } }\npub struct LateWorker;\npub struct OtherWorker;\nimpl OtherWorker { pub fn ambiguous_finish(&self) -> usize { 12 } }\npub enum ProjectResult { Ok, Err }\npub mod inner { pub fn nested_only() -> usize { 7 } }\npub const RUST_LIMIT: usize = 13;\npub fn rust_helper() -> usize { 1 }\npub(self) fn hidden() -> usize { 9 }\n",
        ),
        ("nested/mod.rs", "pub fn nested_helper() -> usize { 2 }\n"),
        ("conflict.rs", "pub fn conflict_helper() -> usize { 3 }\n"),
        (
            "conflict/mod.rs",
            "pub fn conflict_helper() -> usize { 4 }\n",
        ),
        (
            "index_only/index.rs",
            "pub fn index_helper() -> usize { 5 }\n",
        ),
        (
            "lib.rs",
            "mod conflict;\nmod grouped_use;\nmod index_only;\nmod nested;\nmod rust_helper;\nmod walk;\npub use rust_helper::LateWorker;\nuse external_crate::Remote;\npub fn rust_use() -> usize { nested::nested_helper() + rust_helper::rust_helper() + rust_helper::LateWorker::orphan_method() }\npub fn rust_qualified_use() -> usize { crate::rust_helper::LateWorker::orphan_method() }\npub fn rust_root_reexport_use() -> usize { crate::LateWorker::orphan_method() }\npub fn rust_macro_use() { print!(\"{}\", rust_helper::rust_helper()); }\npub fn rust_dynamic_use(worker: &rust_helper::LateWorker) -> usize { worker.unique_finish() + worker.ambiguous_finish() }\npub fn rust_callback_use(enabled: impl Fn() -> bool) -> bool { enabled() }\npub fn rust_expected_boundaries(_remote: &Remote) -> Result<(), ()> { assert!(true); let _values = Vec::new(); let _remote = Remote::default(); let _json = serde_json::to_value(()); Ok(()) }\npub fn rust_rejected() -> usize { rust_helper::hidden() + rust_helper::nested_only() + rust_helper::orphan_method() + rust_helper::inner() + conflict::conflict_helper() + index_only::index_helper() }\n",
        ),
        (
            "grouped_use.rs",
            "use crate::{nested::{self, nested_helper}, rust_helper::{LateWorker, RUST_LIMIT}, rust_helper as helper_alias};\npub fn grouped_rust_use() -> usize { nested::nested_helper() + helper_alias::rust_helper() + nested_helper() + RUST_LIMIT }\nfn apply(callback: fn() -> usize) -> usize { callback() }\npub fn rust_function_item_use() -> usize { apply(LateWorker::orphan_method) }\n",
        ),
        (
            "walk.rs",
            "mod references;\nfn parent_helper() {}\npub fn visit_usage() { references::capture_usage(); self::references::capture_usage(); }\n",
        ),
        (
            "walk/references.rs",
            "use super::*;\npub(super) fn capture_usage() { parent_helper(); }\n",
        ),
        ("py_helpers.py", "def python_helper():\n    return 1\n"),
        (
            "py_consumer.py",
            "from .py_helpers import python_helper\n\ndef python_use():\n    return python_helper()\n",
        ),
        (
            "py_namespace_consumer.py",
            "from . import py_helpers as helpers\n\ndef python_namespace_use():\n    return helpers.python_helper()\n",
        ),
        (
            "go_helpers.go",
            "package fixture\n\nfunc GoHelper() int { return 1 }\n",
        ),
        (
            "go_consumer.go",
            "package fixture\n\nfunc GoUse() int { return GoHelper() }\n",
        ),
        (
            "go_bare.go",
            "package fixture\n\nfunc BareGoCall() int { return Run() + ForeignOnly() + ExternalOnly() + fixture() }\n",
        ),
        (
            "go_external_test.go",
            "package fixture_test\n\nfunc ExternalOnly() int { return 1 }\n",
        ),
        (
            "other/go_foreign.go",
            "package other\n\nfunc ForeignOnly() int { return 1 }\n",
        ),
        (
            "cross_language.ts",
            "import { python_helper } from './py_helpers';\nexport function crossLanguage(): number { return python_helper(); }\n",
        ),
        (
            "external_boundary.ts",
            "import { remote } from 'external-package';\nexport function externalBoundaries(value: Parameters<() => [unknown]>[0]) { remote(); value.map(() => 1); Date.now(); }\n",
        ),
        (
            "fake/mod.ts",
            "export function fake(): number { return 1; }\n",
        ),
        (
            "use_fake.ts",
            "import { fake } from './fake';\nexport function useFake(): number { return fake(); }\n",
        ),
        ("core.ts", "export function core(): number { return 1; }\n"),
        (
            "barrel.ts",
            "const core = (): number => 0;\nexport { core as renamed } from './core';\n",
        ),
        (
            "use_barrel.ts",
            "import { renamed } from './barrel';\nexport function consume(): number { return renamed(); }\n",
        ),
        (
            "local_alias.ts",
            "const internal = (): number => 2;\nexport { internal as publicInternal };\n",
        ),
        (
            "use_local_alias.ts",
            "import { publicInternal } from './local_alias';\nexport function consumeLocalAlias(): number { return publicInternal(); }\n",
        ),
        (
            "default_local.ts",
            "const defaultLocal = (): number => 5;\nexport default defaultLocal;\n",
        ),
        (
            "use_default_local.ts",
            "import selected from './default_local';\nexport function useDefaultLocal(): number { return selected(); }\n",
        ),
        (
            "cjs_helper.js",
            "function cjsHelper() { return 1; }\nexports.cjsHelper = cjsHelper;\n",
        ),
        (
            "cjs_consumer.cjs",
            "const helper = require('./cjs_helper');\nfunction cjsUse() { return helper.cjsHelper(); }\nmodule.exports = { cjsUse };\n",
        ),
        (
            "cjs_destructured.cjs",
            "const { cjsHelper: selectedHelper } = require('./cjs_helper');\nfunction cjsSelectedUse() { return selectedHelper(); }\nmodule.exports = { cjsSelectedUse };\n",
        ),
        (
            "dynamic_require.cjs",
            "function dynamicUse(name) { return require(name); }\nmodule.exports = { dynamicUse };\n",
        ),
        (
            "exported_require.js",
            "export function require() { return 1; }\n",
        ),
        (
            "local_require.cjs",
            "function require(name) { return name; }\nfunction localRequireUse() { return require('local'); }\nmodule.exports = { localRequireUse };\n",
        ),
    ];

    impl ModuleResolverFacts<'_> {
        fn symbol(&self, path: &str, qualified_name: &str) -> SymbolId {
            let file_id = self
                .facts
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .map_or_else(
                    || panic!("missing module resolver file {path}"),
                    |file| &file.file_id,
                );
            let mut symbols = self.facts.symbols().iter().filter(|symbol| {
                &symbol.file_id == file_id && symbol.qualified_name == qualified_name
            });
            let symbol = symbols
                .next()
                .unwrap_or_else(|| panic!("missing module symbol {path}:{qualified_name}"));
            assert!(symbols.next().is_none(), "{path}:{qualified_name}");
            symbol.symbol_id.clone()
        }

        fn parse_implementation(&self) -> SymbolId {
            self.facts
                .documents()
                .iter()
                .find(|document| {
                    document.path() == "src/api.ts"
                        && document.qualified_name() == "parse"
                        && document
                            .metadata_json()
                            .contains("\"declaration_only\":false")
                })
                .and_then(|document| document.symbol_id().cloned())
                .unwrap_or_else(|| panic!("parse implementation document was missing"))
        }

        fn reference(&self, owner: &SymbolId, name: &str) -> &ReferenceInput {
            self.facts
                .references()
                .iter()
                .find(|reference| {
                    reference.owner_symbol_id.as_ref() == Some(owner)
                        && reference.reference_name == name
                })
                .unwrap_or_else(|| panic!("missing reference {name}"))
        }

        fn import_declaration_reference(&self, path: &str, name: &str) -> &ReferenceInput {
            let file_id = self
                .facts
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .map_or_else(
                    || panic!("missing import declaration file {path}"),
                    |file| &file.file_id,
                );
            let file_symbol_id = self
                .facts
                .symbols()
                .iter()
                .find(|symbol| {
                    &symbol.file_id == file_id && symbol.symbol_kind == SymbolKind::File.as_str()
                })
                .map_or_else(
                    || panic!("missing file graph symbol {path}"),
                    |symbol| &symbol.symbol_id,
                );
            let mut references = self.facts.references().iter().filter(|reference| {
                &reference.file_id == file_id
                    && reference.owner_symbol_id.as_ref() == Some(file_symbol_id)
                    && reference.reference_name == name
                    && reference.reference_kind == ReferenceKind::References.as_str()
            });
            let reference = references
                .next()
                .unwrap_or_else(|| panic!("missing import declaration reference {path}:{name}"));
            assert!(references.next().is_none(), "{path}:{name}");
            reference
        }

        fn assert_use_document_terms(&self, owner: &SymbolId) {
            let document = self
                .facts
                .documents()
                .iter()
                .find(|document| document.symbol_id() == Some(owner))
                .unwrap_or_else(|| panic!("use search document was missing"));
            for term in [
                "DefaultClient",
                "RemoteService",
                "parse",
                "api",
                "make",
                "external",
            ] {
                assert!(document.code().contains(term), "{term}");
            }
            assert!(!document.code().contains("'value'"));
        }
    }

    fn config(workers: usize) -> NativePipelineConfig {
        config_with_generation_limit(workers, TEST_GENERATION_BYTES)
    }

    fn config_with_generation_limit(workers: usize, generation_bytes: u64) -> NativePipelineConfig {
        config_with_limits(workers, TEST_SOURCE_BYTES, generation_bytes)
    }

    fn config_with_limits(
        workers: usize,
        source_bytes: usize,
        generation_bytes: u64,
    ) -> NativePipelineConfig {
        let discovery = match DiscoveryLimits::new(TEST_FILES, TEST_PATH_BYTES) {
            Ok(discovery) => discovery,
            Err(error) => panic!("test discovery limits were invalid: {error}"),
        };
        let source = match SourceLimits::new(source_bytes) {
            Ok(source) => source,
            Err(error) => panic!("test source limits were invalid: {error}"),
        };
        let retained = match NativeRetainedLimits::new(TEST_MANIFEST_BYTES, generation_bytes) {
            Ok(retained) => retained,
            Err(error) => panic!("test retained limits were invalid: {error}"),
        };
        let limits = NativePipelineLimits::new(discovery, source, retained);
        let parallelism = match NativePipelineParallelism::new(
            StageCapacity::new(workers, workers),
            StageCapacity::new(workers, workers),
        ) {
            Ok(parallelism) => parallelism,
            Err(error) => panic!("test native pipeline parallelism was invalid: {error}"),
        };
        let deadlines =
            match NativePipelineDeadlines::new(TEST_TIMEOUT, TEST_TIMEOUT, CLEANUP_GRACE) {
                Ok(deadlines) => deadlines,
                Err(error) => panic!("test native pipeline deadlines were invalid: {error}"),
            };
        NativePipelineConfig::new(limits, parallelism, deadlines)
    }

    fn spilled_manifest_entry(index: u64, byte_size: u64) -> SourceManifestEntry {
        let path = NormalizedPath::parse(&format!("src/spilled_{index}.ts"))
            .unwrap_or_else(|error| panic!("spilled manifest path failed: {error}"));
        let mut file_bytes = [0_u8; DOCUMENT_UUID_BYTES];
        file_bytes[DOCUMENT_UUID_BYTES - size_of::<u64>()..].copy_from_slice(&index.to_be_bytes());
        let mut digest_bytes = [0_u8; 32];
        digest_bytes[32 - size_of::<u64>()..].copy_from_slice(&index.to_be_bytes());
        SourceManifestEntry {
            path,
            language: SourceLanguage::TypeScript,
            file_id: FileId::from_uuid_v8(file_bytes),
            content_hash: ContentDigest::from_bytes(digest_bytes),
            byte_size,
        }
    }

    fn spilled_parse_test_config() -> NativePipelineConfig {
        let mut config = config(SERIAL_WORKERS);
        config.deadlines =
            NativePipelineDeadlines::new(Duration::from_millis(500), TEST_TIMEOUT, CLEANUP_GRACE)
                .unwrap_or_else(|error| panic!("spilled parse deadlines failed: {error}"));
        config
    }

    #[test]
    fn spilled_parse_envelopes_are_lazy_and_cap_each_batch_by_file_count() {
        let manifest = (0..65)
            .map(|index| spilled_manifest_entry(index, 1))
            .collect::<Vec<_>>();
        let stage_deadline = Instant::now() + TEST_TIMEOUT;
        let (mut envelopes, failed) =
            spilled_parse_envelopes(manifest, spilled_parse_test_config(), stage_deadline);
        let first = envelopes
            .next()
            .unwrap_or_else(|| panic!("first spilled parse envelope was missing"));
        assert_eq!(first.test_payload().entries.len(), 64);
        assert_eq!(first.test_meta().budget().progress_bytes(), 64);
        let first_deadline = first.test_meta().budget().deadline();

        std::thread::sleep(Duration::from_millis(20));
        let second = envelopes
            .next()
            .unwrap_or_else(|| panic!("second spilled parse envelope was missing"));
        assert_eq!(second.test_payload().entries.len(), 1);
        assert_eq!(second.test_meta().budget().progress_bytes(), 1);
        assert!(second.test_meta().budget().deadline() > first_deadline);
        assert!(envelopes.next().is_none());
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn spilled_parse_envelopes_cap_combined_source_bytes() {
        let mebibyte = 1024_u64 * 1024;
        let manifest = [40 * mebibyte, 30 * mebibyte, mebibyte]
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| spilled_manifest_entry(usize_to_u64(index), bytes))
            .collect::<Vec<_>>();
        let stage_deadline = Instant::now() + TEST_TIMEOUT;
        let (mut envelopes, failed) =
            spilled_parse_envelopes(manifest, spilled_parse_test_config(), stage_deadline);
        let first = envelopes
            .next()
            .unwrap_or_else(|| panic!("first byte-bounded envelope was missing"));
        assert_eq!(first.test_payload().entries.len(), 1);
        assert_eq!(first.test_meta().budget().progress_bytes(), 40 * mebibyte);
        let second = envelopes
            .next()
            .unwrap_or_else(|| panic!("second byte-bounded envelope was missing"));
        assert_eq!(second.test_payload().entries.len(), 2);
        assert_eq!(second.test_meta().budget().progress_bytes(), 31 * mebibyte);
        assert!(envelopes.next().is_none());
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn spilled_parse_envelopes_keep_one_oversized_file_indivisible() {
        let mebibyte = 1024_u64 * 1024;
        let manifest = [70 * mebibyte, mebibyte]
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| spilled_manifest_entry(usize_to_u64(index), bytes))
            .collect::<Vec<_>>();
        let stage_deadline = Instant::now() + TEST_TIMEOUT;
        let (mut envelopes, failed) =
            spilled_parse_envelopes(manifest, spilled_parse_test_config(), stage_deadline);
        let oversized = envelopes
            .next()
            .unwrap_or_else(|| panic!("oversized envelope was missing"));
        assert_eq!(oversized.test_payload().entries.len(), 1);
        assert_eq!(
            oversized.test_meta().budget().progress_bytes(),
            70 * mebibyte
        );
        let following = envelopes
            .next()
            .unwrap_or_else(|| panic!("post-oversized envelope was missing"));
        assert_eq!(following.test_payload().entries.len(), 1);
        assert_eq!(following.test_meta().budget().progress_bytes(), mebibyte);
        assert!(envelopes.next().is_none());
        assert!(!failed.load(Ordering::Acquire));
    }

    async fn parse_project_through_native_stages(
        stages: &NativeStageContext<'_>,
    ) -> NativeFactAccumulator {
        let (discovered, _) = run_discovery_stage(stages)
            .await
            .unwrap_or_else(|error| panic!("discovery failed: {error}"));
        let (manifest, _) = run_read_stage(stages, discovered)
            .await
            .unwrap_or_else(|error| panic!("read stage failed: {error}"));
        run_parse_stage(stages, manifest.entries, None)
            .await
            .map_or_else(
                |error| panic!("parse stage failed: {error}"),
                |(facts, _)| facts,
            )
    }

    fn write_project(root: &std::path::Path) {
        assert!(fs::create_dir(root.join(".git")).is_ok());
        assert!(fs::create_dir_all(root.join("src/ignored")).is_ok());
        assert!(fs::write(root.join(".gitignore"), "src/ignored/\n").is_ok());
        assert!(fs::write(root.join("src/ignored/no.ts"), "export const no = 1;\n").is_ok());
        assert!(
            fs::write(
                root.join("src/service.ts"),
                "export class Base {}\nexport interface Greeter {}\nexport function format(): string { return 'ok'; }\nexport class Service extends Base implements Greeter {\n  greet(): string { return format(); }\n  use(): void { this.greet; }\n}\n",
            )
            .is_ok()
        );
        assert!(
            fs::write(
                root.join("src/build.ts"),
                "import { Service } from './service';\nexport function build(): Service { return new Service(); }\n",
            )
            .is_ok()
        );
        assert!(
            fs::write(
                root.join("src/view.test.tsx"),
                "export function View(): JSX.Element { return <Service />; }\n",
            )
            .is_ok()
        );
    }

    fn write_module_project(root: &std::path::Path) {
        assert!(fs::create_dir(root.join(".git")).is_ok());
        assert!(fs::create_dir_all(root.join("src/foo")).is_ok());
        assert!(
            fs::write(
                root.join("src/api.ts"),
                "export default class DefaultClient {}\n\
                 export function Service(): void {}\n\
                 export function parse(value: string): string;\n\
                 export function parse(value: number): string;\n\
                 export function parse(value: unknown): string { return String(value); }\n\
                 export function make(): void {}\n",
            )
            .is_ok()
        );
        assert!(
            fs::write(
                root.join("src/other.ts"),
                "export function external(): void {}\n\
                 export function Service(): void {}\n",
            )
            .is_ok()
        );
        assert!(
            fs::write(
                root.join("src/consumer.ts"),
                "import DefaultClient, { Service as RemoteService, parse } from './api';\n\
                 import * as api from './api';\n\
                 import { external } from 'package';\n\
                 function Service(): void {}\n\
                 export function use(): void {\n\
                   new DefaultClient();\n\
                   RemoteService();\n\
                   parse('value');\n\
                   api.make();\n\
                   external();\n\
                 }\n\
                 export function shadow(): void {\n\
                   const RemoteService = (): void => {};\n\
                   RemoteService();\n\
                 }\n",
            )
            .is_ok()
        );
        assert!(
            fs::write(
                root.join("src/members.ts"),
                "export function run(): void {}\n\
                 export class Worker {\n\
                   run(): void {}\n\
                   execute(): void { run(); }\n\
                 }\n",
            )
            .is_ok()
        );
        for path in ["src/foo.ts", "src/foo.js", "src/foo/index.ts"] {
            assert!(fs::write(root.join(path), "export function choose(): void {}\n",).is_ok());
        }
        assert!(
            fs::write(
                root.join("src/ambiguous.ts"),
                "import { choose } from './foo';\n\
                 export function useAmbiguous(): void { choose(); }\n",
            )
            .is_ok()
        );
    }

    async fn build(root: &std::path::Path, workers: usize) -> NativeGeneration {
        build_with_config(root, workers, config(workers)).await
    }

    async fn build_with_config(
        root: &std::path::Path,
        workers: usize,
        config: NativePipelineConfig,
    ) -> NativeGeneration {
        let (runner, tasks, cancellation) = test_stage_runner(
            workers.saturating_mul(2).saturating_add(4),
            TEST_SCOPE_BYTES,
        )
        .await;
        let source_root = match SourceRoot::open(root) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open pipeline fixture: {error}"),
        };
        let generation = match build_native_generation(&runner, source_root, config).await {
            Ok(generation) => generation,
            Err(error) => panic!("native pipeline failed: {error}"),
        };
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
        assert!(!report.unobserved_results);
        generation
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn numerical_sites_survive_resolution_and_are_worker_invariant() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create fixture: {error}"));
        fs::create_dir_all(directory.path().join("src"))
            .unwrap_or_else(|error| panic!("could not create numerical fixture: {error}"));
        fs::write(
            directory.path().join("src/numerics.rs"),
            r"
pub fn score(a: u16, b: u16, value: f32) -> f32 {
    let widened = (a * b) as f32;
    let root = value.sqrt();
    if (root - widened).abs() < 1e-6 {
        root.max(widened)
    } else {
        value / widened
    }
}
",
        )
        .unwrap_or_else(|error| panic!("could not write numerical fixture: {error}"));

        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        assert_eq!(
            serial.facts().numerical_sites(),
            parallel.facts().numerical_sites()
        );
        assert_eq!(
            serial.facts().digest_version(),
            cartograph_domain::GenerationDigestVersion::CURRENT
        );
        assert_eq!(
            serial.report().numerical_sites(),
            usize_to_u64(serial.facts().numerical_sites().len())
        );
        let hazards = serial
            .facts()
            .numerical_sites()
            .iter()
            .map(|site| site.hazard.as_str())
            .collect::<BTreeSet<_>>();
        for expected in [
            "arithmetic_before_widening",
            "absolute_only_tolerance",
            "domain_precondition_unknown",
            "nan_ordering_unknown",
        ] {
            assert!(hazards.contains(expected), "missing hazard {expected}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn oversized_sources_are_skipped_and_counted_without_failing_the_generation() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create fixture: {error}"));
        fs::write(
            directory.path().join("small.ts"),
            "export const small = 1;\n",
        )
        .unwrap_or_else(|error| panic!("could not write small fixture: {error}"));
        fs::write(directory.path().join("large.ts"), "x".repeat(2 * 1024))
            .unwrap_or_else(|error| panic!("could not write oversized fixture: {error}"));

        let generation = build_with_config(
            directory.path(),
            SERIAL_WORKERS,
            config_with_limits(SERIAL_WORKERS, 1024, TEST_GENERATION_BYTES),
        )
        .await;
        assert_eq!(generation.report().discovered_files(), 1);
        assert_eq!(generation.report().skipped_oversized_files(), 1);
        assert_eq!(generation.facts().files().len(), 1);
        assert_eq!(generation.facts().files()[0].normalized_path, "small.ts");
    }

    fn exact_scip_overlay_fixture() -> Vec<u8> {
        let source_scip_symbol = "cartograph cartograph demo 1 `src/main.rs`/caller().";
        let target_scip_symbol = "cartograph cartograph demo 1 `src/main.rs`/callee().";
        encode_scip_index(&ScipIndex {
            tool_name: "cartograph".to_owned(),
            tool_version: "2.0.0".to_owned(),
            project_root: "cartograph://demo".to_owned(),
            documents: vec![ScipDocument {
                relative_path: "src/main.rs".to_owned(),
                language: "rust".to_owned(),
                occurrences: vec![
                    ScipOccurrence {
                        range: vec![0, 0, 25],
                        symbol: source_scip_symbol.to_owned(),
                        symbol_roles: SYMBOL_ROLE_DEFINITION,
                        enclosing_range: vec![0, 0, 25],
                    },
                    ScipOccurrence {
                        range: vec![0, 14, 20],
                        symbol: target_scip_symbol.to_owned(),
                        symbol_roles: 0,
                        enclosing_range: vec![0, 0, 25],
                    },
                    ScipOccurrence {
                        range: vec![1, 0, 14],
                        symbol: target_scip_symbol.to_owned(),
                        symbol_roles: SYMBOL_ROLE_DEFINITION,
                        enclosing_range: vec![1, 0, 14],
                    },
                ],
                symbols: vec![
                    ScipSymbolInformation {
                        symbol: source_scip_symbol.to_owned(),
                        display_name: "caller".to_owned(),
                        kind: 17,
                        documentation: vec!["Entry point".to_owned()],
                        relationships: Vec::new(),
                        enclosing_symbol: String::new(),
                        cartograph_edges: vec![CartographScipEdge {
                            target_symbol: target_scip_symbol.to_owned(),
                            edge_kind: "calls".to_owned(),
                            site_count: 9,
                            provenance: "scip-fixture".to_owned(),
                            confidence_bits: 0.9_f32.to_bits(),
                        }],
                    },
                    ScipSymbolInformation {
                        symbol: target_scip_symbol.to_owned(),
                        display_name: "callee".to_owned(),
                        kind: 17,
                        documentation: Vec::new(),
                        relationships: Vec::new(),
                        enclosing_symbol: String::new(),
                        cartograph_edges: Vec::new(),
                    },
                ],
            }],
        })
        .unwrap_or_else(|error| panic!("SCIP fixture encode failed: {error}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn persistent_scip_overlay_replaces_covered_graph_with_exact_typed_edges() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create SCIP fixture: {error}"));
        assert!(fs::create_dir(directory.path().join(".git")).is_ok());
        assert!(fs::create_dir(directory.path().join("src")).is_ok());
        let source = "fn caller() { callee(); }\nfn callee() {}\n";
        assert!(fs::write(directory.path().join("src/main.rs"), source).is_ok());
        let overlay = ScipOverlayInput::new(exact_scip_overlay_fixture(), 100)
            .unwrap_or_else(|error| panic!("SCIP overlay config failed: {error}"));
        let (runner, tasks, cancellation) = test_stage_runner(8, TEST_SCOPE_BYTES).await;
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open SCIP fixture: {error}"));
        let generation = build_native_generation_with_scip(
            &runner,
            NativeGenerationBuild::new(source_root, config(PARALLEL_WORKERS))
                .with_scip_overlay(overlay),
        )
        .await
        .unwrap_or_else(|error| panic!("SCIP pipeline failed: {error}"));
        drop(cancellation);
        let task_report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(task_report.all_joined);
        assert!(!task_report.worker_failed);
        let report = generation
            .report()
            .scip_overlay()
            .unwrap_or_else(|| panic!("SCIP report was missing"));
        assert_eq!(report.covered_documents(), 1);
        assert_eq!(report.exact_typed_edges(), 1);
        let source_id = generation
            .facts()
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == "caller")
            .map_or_else(
                || panic!("SCIP caller was missing"),
                |symbol| &symbol.symbol_id,
            );
        let target_id = generation
            .facts()
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == "callee")
            .map_or_else(
                || panic!("SCIP callee was missing"),
                |symbol| &symbol.symbol_id,
            );
        assert!(generation.facts().edges().iter().any(|edge| {
            &edge.source_symbol_id == source_id
                && &edge.target_symbol_id == target_id
                && edge.kind == EdgeKind::Calls
                && edge.site_count == 9
                && edge.provenance == "scip-overlay:scip-fixture"
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pipeline_is_gitignore_aware_resolves_unique_symbols_and_is_worker_deterministic() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create pipeline fixture: {error}"),
        };
        write_project(directory.path());
        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.report(), parallel.report());
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        assert_eq!(serial.report().discovered_files(), EXPECTED_SOURCE_FILES);
        assert!(serial.report().symbols() >= EXPECTED_MINIMUM_SYMBOLS);
        assert!(serial.report().resolved_references() >= EXPECTED_MINIMUM_RESOLVED_REFERENCES);
        assert!(
            serial
                .facts()
                .edges()
                .iter()
                .any(|edge| edge.kind == EdgeKind::Implements)
        );
        assert!(
            serial
                .facts()
                .edges()
                .iter()
                .any(|edge| edge.kind == EdgeKind::Instantiates)
        );
        assert!(serial.facts().references().iter().any(|reference| {
            reference.target_symbol_id.is_none()
                && !reference.reference_name.is_empty()
                && reference.owner_symbol_id.is_some()
                && reference.resolution_provenance == UNRESOLVED_PROVENANCE
        }));
        assert!(
            serial
                .facts()
                .documents()
                .iter()
                .any(|document| document.kind() == DocumentKind::Test)
        );
        let reference_kinds = serial
            .facts()
            .references()
            .iter()
            .map(|reference| reference.reference_kind.as_str())
            .collect::<BTreeSet<_>>();
        for expected in [
            "calls",
            "imports",
            "references",
            "implements",
            "extends",
            "type_of",
            "returns",
            "instantiates",
            "field_access",
        ] {
            assert!(reference_kinds.contains(expected), "{expected}");
        }
        let edge_kinds = serial
            .facts()
            .edges()
            .iter()
            .map(|edge| edge.kind.as_str())
            .collect::<BTreeSet<_>>();
        for expected in [
            "calls",
            "references",
            "implements",
            "extends",
            "type_of",
            "returns",
            "instantiates",
            "field_access",
            "contains",
        ] {
            assert!(edge_kinds.contains(expected), "{expected}");
        }
        assert_eq!(
            serial.facts().documents().len(),
            serial.facts().symbols().len()
        );
        let debug = format!("{serial:?}");
        assert!(!debug.contains("Service"));
        assert!(!debug.contains("src/service.ts"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parser_only_modes_publish_file_documents_with_a_locked_worker_invariant_digest() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create parser-only fixture: {error}"));
        write_parser_only_project(directory.path());
        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;

        assert_eq!(serial.report(), parallel.report());
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        assert_generation_projection(serial.facts(), EXPECTED_PARSER_ONLY_PROJECTION);
        assert_eq!(
            serial.facts().digest().as_str(),
            EXPECTED_PARSER_ONLY_DIGEST
        );
        assert_parser_only_generation(serial.facts());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn admitted_families_are_worker_invariant_structural_and_searchable() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create admitted-family fixture: {error}"));
        write_admitted_family_project(directory.path());
        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        assert_generation_projection(serial.facts(), EXPECTED_ADMITTED_FAMILY_PROJECTION);
        assert_eq!(
            serial.facts().digest().as_str(),
            EXPECTED_ADMITTED_FAMILY_DIGEST
        );
        assert_eq!(serial.facts().files().len(), ADMITTED_FAMILY_FILE_COUNT);
        for (path, _, language) in ADMITTED_FAMILY_FIXTURES {
            assert!(language.is_native_indexable(), "{}", language.as_str());
            let file = serial
                .facts()
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .unwrap_or_else(|| panic!("missing admitted-family file {path}"));
            assert_eq!(file.language, language.as_str());
            let symbols = serial
                .facts()
                .symbols()
                .iter()
                .filter(|symbol| {
                    symbol.file_id == file.file_id
                        && symbol.symbol_kind != SymbolKind::File.as_str()
                })
                .collect::<Vec<_>>();
            assert!(!symbols.is_empty(), "no structural symbols for {path}");
            assert!(symbols.iter().any(|symbol| {
                serial.facts().documents().iter().any(|document| {
                    document.symbol_id() == Some(&symbol.symbol_id)
                        && document.path() == path
                        && !document.code().is_empty()
                })
            }));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn generic_families_are_worker_invariant_on_real_v1_corpora() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create generic-family fixture: {error}"));
        write_generic_family_project(directory.path());
        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        assert_generation_projection(serial.facts(), EXPECTED_GENERIC_FAMILY_PROJECTION);
        assert_eq!(
            serial.facts().digest().as_str(),
            EXPECTED_GENERIC_FAMILY_DIGEST
        );
        assert_eq!(serial.facts().files().len(), GENERIC_FAMILY_FILE_COUNT);
        for (path, _, language) in GENERIC_FAMILY_FIXTURES {
            let file = serial
                .facts()
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .unwrap_or_else(|| panic!("missing generic-family file {path}"));
            assert_eq!(file.language, language.as_str());
            assert!(
                serial
                    .facts()
                    .documents()
                    .iter()
                    .any(|document| { document.path() == path && document.symbol_id().is_some() })
            );
            if language != SourceLanguage::Html {
                assert!(
                    serial.facts().symbols().iter().any(|symbol| {
                        symbol.file_id == file.file_id
                            && symbol.symbol_kind != SymbolKind::File.as_str()
                    }),
                    "no structural symbols for {path}"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn custom_families_are_worker_invariant_structural_and_searchable() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create custom-family fixture: {error}"));
        write_custom_family_project(directory.path());
        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.report(), parallel.report());
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        assert_generation_projection(serial.facts(), EXPECTED_CUSTOM_FAMILY_PROJECTION);
        assert_eq!(
            serial.facts().digest().as_str(),
            EXPECTED_CUSTOM_FAMILY_DIGEST
        );
        assert_eq!(serial.facts().files().len(), CUSTOM_FAMILY_FILE_COUNT);
        for (path, _, language) in CUSTOM_FAMILY_FIXTURES {
            assert!(language.is_native_indexable(), "{}", language.as_str());
            let file = serial
                .facts()
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .unwrap_or_else(|| panic!("missing custom-family file {path}"));
            assert_eq!(file.language, language.as_str());
            assert!(serial.facts().symbols().iter().any(|symbol| {
                symbol.file_id == file.file_id && symbol.symbol_kind != SymbolKind::File.as_str()
            }));
            assert!(serial.facts().documents().iter().any(|document| {
                document.path() == path
                    && document.symbol_id().is_some()
                    && document.kind() == DocumentKind::Symbol
            }));
        }
    }

    #[test]
    fn rhai_literal_modules_and_namespace_calls_resolve_without_execution() {
        let fixtures = [
            (
                "scripts/crypto.rhai",
                "fn encrypt(value) { value }\nprivate fn hidden(value) { value }\n",
            ),
            (
                "scripts/main.rhai",
                "import \"crypto\" as lock;\nfn call_crypto(value) { lock::encrypt(value) }\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        assert_eq!(forward.references(), reversed.references());
        assert_eq!(forward.edges(), reversed.edges());

        let caller = capability_symbol(&forward, "scripts/main.rhai", "call_crypto");
        let target = capability_symbol(&forward, "scripts/crypto.rhai", "encrypt");
        let reference = CapabilityReferenceQuery::new(&forward, caller)
            .named("lock::encrypt", ReferenceKind::Calls);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == caller.symbol_id
                && edge.target_symbol_id == target.symbol_id
                && edge.kind == EdgeKind::Calls
        }));

        let hidden = capability_symbol(&forward, "scripts/crypto.rhai", "hidden");
        assert!(!hidden.export.exported);
        assert_eq!(hidden.visibility, Some(Visibility::Private));
    }

    #[test]
    fn game_script_module_paths_resolve_relative_and_engine_root_imports() {
        let fixtures = [
            ("wren/helper.wren", "class Helper { }\n"),
            (
                "wren/main.wren",
                "import \"./helper\" for Helper\nclass Main { }\n",
            ),
            ("scripts/helper.gd", "class_name Helper\n"),
            (
                "scripts/main.gd",
                "class_name Main\nconst Helper = preload(\"res://scripts/helper.gd\")\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        assert_eq!(forward.references(), reversed.references());
        assert_eq!(forward.edges(), reversed.edges());

        assert_import_targets_file(&forward, "wren/main.wren", "./helper", "wren/helper.wren");
        assert_import_targets_file(
            &forward,
            "scripts/main.gd",
            "res://scripts/helper.gd",
            "scripts/helper.gd",
        );
    }

    fn assert_import_targets_file(
        facts: &CanonicalGenerationFacts,
        source_path: &str,
        module: &str,
        target_path: &str,
    ) {
        let source = capability_file_symbol(facts, source_path);
        let target = capability_file_symbol(facts, target_path);
        let reference =
            CapabilityReferenceQuery::new(facts, source).named(module, ReferenceKind::Imports);
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(reference.resolution_provenance, MODULE_IMPORT_PROVENANCE);
        assert!(facts.edges().iter().any(|edge| {
            edge.source_symbol_id == source.symbol_id
                && edge.target_symbol_id == target.symbol_id
                && edge.kind == EdgeKind::Imports
        }));
    }

    #[test]
    fn c_family_capability_facts_resolve_files_headers_and_implementations_deterministically() {
        let forward = build_capability_generation(&C_RESOLVER_FIXTURES, false);
        let reversed = build_capability_generation(&C_RESOLVER_FIXTURES, true);
        assert_eq!(forward.digest(), reversed.digest());
        assert_eq!(forward.files(), reversed.files());
        assert_eq!(forward.symbols(), reversed.symbols());
        assert_eq!(forward.edges(), reversed.edges());
        assert_eq!(forward.references(), reversed.references());

        let main_file = capability_file_symbol(&forward, "src/main.cpp");
        let header_file = capability_file_symbol(&forward, "include/api.hpp");
        let api_declaration = capability_symbol(&forward, "include/api.hpp", "api::make_worker");
        let ping_implementation = capability_symbol(&forward, "src/api.cpp", "api::Worker::ping");
        let worker_type = capability_symbol(&forward, "include/api.hpp", "api::Worker");
        let shared_implementation = capability_symbol(&forward, "src/api.cpp", "api::shared_value");
        let use_api = capability_symbol(&forward, "src/main.cpp", "use_api");

        let include = CapabilityReferenceQuery::new(&forward, main_file)
            .named("../include/api.hpp", ReferenceKind::Imports);
        assert_eq!(
            include.target_symbol_id.as_ref(),
            Some(&header_file.symbol_id)
        );
        assert_eq!(include.resolution_provenance, QUOTED_INCLUDE_PROVENANCE);
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == main_file.symbol_id
                && edge.target_symbol_id == header_file.symbol_id
                && edge.kind == EdgeKind::Imports
        }));

        for (name, kind, target) in [
            ("api::make_worker", ReferenceKind::Calls, api_declaration),
            (
                "api::Worker::ping",
                ReferenceKind::Calls,
                ping_implementation,
            ),
            ("api::Worker", ReferenceKind::Instantiates, worker_type),
            (
                "api::shared_value",
                ReferenceKind::Calls,
                shared_implementation,
            ),
        ] {
            let reference = CapabilityReferenceQuery::new(&forward, use_api).named(name, kind);
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
            assert_eq!(reference.resolution_provenance, QUOTED_INCLUDE_PROVENANCE);
        }

        for name in ["worker.run", "ping", "api::Worker::private_run"] {
            let reference =
                CapabilityReferenceQuery::new(&forward, use_api).named(name, ReferenceKind::Calls);
            assert!(
                reference.target_symbol_id.is_none(),
                "{name}: {reference:?}"
            );
            assert_ne!(reference.resolution_provenance, QUOTED_INCLUDE_PROVENANCE);
        }

        let system_include = CapabilityReferenceQuery::new(&forward, main_file)
            .named("vector", ReferenceKind::Imports);
        assert!(system_include.target_symbol_id.is_none());
        assert_eq!(
            system_include.resolution_provenance,
            UNRESOLVED_IMPORT_PROVENANCE
        );
        let negative = capability_symbol(&forward, "src/negative.cpp", "unresolved_use");
        for name in ["orphan", "duplicate", "missing_system"] {
            let reference =
                CapabilityReferenceQuery::new(&forward, negative).named(name, ReferenceKind::Calls);
            assert!(reference.target_symbol_id.is_none(), "{name}");
        }
        let missing_file = capability_file_symbol(&forward, "src/missing.cpp");
        let missing_include = CapabilityReferenceQuery::new(&forward, missing_file)
            .named("../include/missing.hpp", ReferenceKind::Imports);
        assert!(missing_include.target_symbol_id.is_none());
        assert_eq!(
            missing_include.resolution_provenance,
            UNRESOLVED_IMPORT_PROVENANCE
        );

        for path in C_RESOLVER_FIXTURES.map(|(path, _)| path) {
            let file = capability_file_symbol(&forward, path);
            assert!(forward.edges().iter().any(|edge| {
                edge.source_symbol_id == file.symbol_id && edge.kind == EdgeKind::Contains
            }));
            assert!(forward.documents().iter().any(|document| {
                document.path() == path && document.symbol_id() == Some(&file.symbol_id)
            }));
        }
        assert!(SourceLanguage::C.is_native_indexable());
        assert!(SourceLanguage::Cpp.is_native_indexable());
    }

    #[test]
    fn csharp_base_edges_follow_resolved_target_kinds_without_name_guessing() {
        let forward = build_capability_generation(&CSHARP_RESOLVER_FIXTURES, false);
        let reversed = build_capability_generation(&CSHARP_RESOLVER_FIXTURES, true);
        assert_eq!(forward.digest(), reversed.digest());
        assert_eq!(forward.references(), reversed.references());
        assert_eq!(forward.edges(), reversed.edges());

        let device = capability_symbol(&forward, "src/Bases.cs", "Device");
        let iphone = capability_symbol(&forward, "src/Bases.cs", "IPhone");
        let disposable = capability_symbol(&forward, "src/Bases.cs", "Disposable");
        let missing_device = capability_symbol(&forward, "src/Bases.cs", "MissingDevice");

        let iphone_reference = CapabilityReferenceQuery::new(&forward, device)
            .named("IPhone", ReferenceKind::Inherits);
        assert_eq!(
            iphone_reference.target_symbol_id.as_ref(),
            Some(&iphone.symbol_id)
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == device.symbol_id
                && edge.target_symbol_id == iphone.symbol_id
                && edge.kind == EdgeKind::Extends
        }));

        let disposable_reference = CapabilityReferenceQuery::new(&forward, device)
            .named("Disposable", ReferenceKind::Inherits);
        assert_eq!(
            disposable_reference.target_symbol_id.as_ref(),
            Some(&disposable.symbol_id)
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == device.symbol_id
                && edge.target_symbol_id == disposable.symbol_id
                && edge.kind == EdgeKind::Implements
        }));

        let missing = CapabilityReferenceQuery::new(&forward, missing_device)
            .named("MissingBase", ReferenceKind::Inherits);
        assert!(missing.target_symbol_id.is_none());
        assert_eq!(missing.resolution_provenance, UNRESOLVED_PROVENANCE);
        assert!(forward.edges().iter().all(|edge| {
            edge.source_symbol_id != missing_device.symbol_id
                || !matches!(edge.kind, EdgeKind::Extends | EdgeKind::Implements)
        }));
        assert!(SourceLanguage::CSharp.is_native_indexable());
    }

    #[test]
    fn framework_bridges_resolve_properties_mybatis_salesforce_and_component_edges() {
        let fixtures = [
            ("config/application.properties", "orders.cache.ttl=30\n"),
            (
                "src/OrderConfig.java",
                "import org.springframework.beans.factory.annotation.Value;\npublic class OrderConfig { @Value(\"${orders.cache.ttl}\") private String ttl; }\n",
            ),
            (
                "src/OrderMapper.java",
                "public interface OrderMapper { Order findOrder(String orderId); }\n",
            ),
            (
                "mappers/OrderMapper.xml",
                "<mapper namespace=\"com.example.OrderMapper\"><select id=\"findOrder\">SELECT 1</select></mapper>\n",
            ),
            (
                "force-app/main/default/classes/OrderController.cls",
                "public class OrderController { public static void loadOrders() {} }\n",
            ),
            (
                "force-app/main/default/pages/Orders.page",
                "<apex:page controller=\"OrderController\" action=\"{!loadOrders}\"/>\n",
            ),
            (
                "src/OrderCard.tsx",
                "export function OrderCard() { return <div />; }\n",
            ),
            (
                "src/OrderPanel.svelte",
                "<script>export function load() {}</script><OrderCard />\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        NamedReferenceAssertion::new(&forward, "orders.cache.ttl")
            .targets("config/application.properties", "orders.cache.ttl");
        let java_mapper =
            capability_symbol(&forward, "src/OrderMapper.java", "OrderMapper::findOrder");
        let xml_statement = capability_symbol(
            &forward,
            "mappers/OrderMapper.xml",
            "OrderMapper::findOrder",
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == java_mapper.symbol_id
                && edge.target_symbol_id == xml_statement.symbol_id
                && edge.kind == EdgeKind::References
                && edge.provenance == MYBATIS_BRIDGE_PROVENANCE
        }));
        NamedReferenceAssertion::new(&forward, "loadOrders").targets(
            "force-app/main/default/classes/OrderController.cls",
            "OrderController::loadOrders",
        );
        NamedReferenceAssertion::new(&forward, "OrderCard")
            .targets("src/OrderCard.tsx", "OrderCard");
    }

    const JVM_FRAMEWORK_BINDING_FIXTURES: [(&str, &str); 18] = [
        (
            "src/main/java/com/audit/AuditMapper.java",
            "package com.audit;\npublic interface AuditMapper { void record(@Param(\"entry\") String entry); }\n",
        ),
        (
            "generated/com/audit/AuditMapper.java",
            "package com.audit;\npublic interface AuditMapper { void record(@Param(\"entry\") String entry); }\n",
        ),
        (
            "src/main/java/AuditMapper.java",
            "public interface AuditMapper { void record(@Param(\"entry\") String entry); }\n",
        ),
        (
            "src/main/resources/mapper/AuditMapper.xml",
            "<mapper namespace=\"com.audit.AuditMapper\"><insert id=\"record\">INSERT INTO audit VALUES (#{entry})</insert></mapper>\n",
        ),
        (
            "src/main/java/OrderMapper.java",
            "public interface OrderMapper { Order findOrder(String orderId); }\n",
        ),
        (
            "force-app/main/default/triggers/AccountTrigger.trigger",
            "trigger AccountTrigger on Account (before insert) {\n    for (Contact c : [SELECT Id FROM Contact]) { AccountService.load(); }\n}\n",
        ),
        (
            "src/Person.vb",
            "Public Class Person\n  Inherits BasePerson\n  Implements IGreeter, IDisposable\n  Public Sub Run()\n    Helper()\n  End Sub\nEnd Class\n",
        ),
        (
            "src/main/java/com/example/OrderMapper.java",
            "package com.example;\npublic interface OrderMapper { Order findOrder(@Param(\"orderId\") String orderId); }\n",
        ),
        (
            "src/main/java/com/other/OrderMapper.java",
            "package com.other;\npublic interface OrderMapper { Order findOrder(@Param(\"orderId\") String orderId); }\n",
        ),
        (
            "src/main/resources/mapper/OrderMapper.xml",
            "<mapper namespace=\"com.example.OrderMapper\"><select id=\"findOrder\">SELECT 1 WHERE id = #{orderId}</select></mapper>\n",
        ),
        (
            "src/main/kotlin/UserMapper.kt",
            "interface UserMapper {\n  fun findById(@Param(\"userId\") userId: Long): User\n}\n",
        ),
        (
            "src/main/resources/mapper/UserMapper.xml",
            "<mapper namespace=\"com.example.UserMapper\"><select id=\"findById\">SELECT 1 WHERE id = #{userId}</select></mapper>\n",
        ),
        (
            "src/main/java/com/example/order/dao/impl/OrderAttributeDaoImpl.java",
            "package com.example.order.dao.impl;\nimport com.example.order.dao.OrderAttributeDao;\npublic class OrderAttributeDaoImpl {\n  private static final String SQL_NS = OrderAttributeDao.class.getName() + \"Mapper\";\n  public void deleteByOrderId(String orderId) {\n    getSqlSessionTemplate().delete(SQL_NS + \".deleteByOrderId\", orderId);\n  }\n}\n",
        ),
        (
            "src/main/resources/mapper/OrderAttributeDaoMapper.xml",
            "<mapper namespace=\"com.example.order.dao.OrderAttributeDaoMapper\">\n  <delete id=\"deleteByOrderId\">DELETE FROM order_attribute WHERE order_id = #{orderId}</delete>\n</mapper>\n",
        ),
        (
            "config/application.properties",
            "feature.payments.enabled=true\napp.cache.ttl=60\n",
        ),
        (
            "src/main/java/com/example/PaymentsAutoConfig.java",
            "package com.example;\n@ConditionalOnProperty(prefix = \"feature.payments\", name = \"enabled\", havingValue = \"true\")\npublic class PaymentsAutoConfig {\n  @Value(\"${app.cache.ttl}\")\n  private int cacheTtl;\n}\n",
        ),
        (
            "src/main/kotlin/Ext.kt",
            "package com.demo\nclass Box\nfun Box.open(): Box = this\nfun String.shout(): String = this\nfun String.echo(): String = shout()\n",
        ),
        (
            "src/main/kotlin/Other.kt",
            "package com.demo\nfun String.loud(): String = shout()\n",
        ),
    ];

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn jvm_framework_bindings_are_worker_count_invariant() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create JVM binding fixture: {error}"));
        for (path, source) in JVM_FRAMEWORK_BINDING_FIXTURES {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("JVM binding fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
        let serial = build(directory.path(), SERIAL_WORKERS).await;
        for workers in [2, PARALLEL_WORKERS, 8, 16] {
            let parallel = build(directory.path(), workers).await;
            assert_eq!(
                serial.facts().digest(),
                parallel.facts().digest(),
                "{workers} workers changed the JVM framework bindings"
            );
        }
    }

    fn jvm_binding_generation() -> CanonicalGenerationFacts {
        let forward = build_capability_generation(&JVM_FRAMEWORK_BINDING_FIXTURES, false);
        let reversed = build_capability_generation(&JVM_FRAMEWORK_BINDING_FIXTURES, true);
        assert_eq!(forward.digest(), reversed.digest());
        forward
    }

    fn jvm_binding_edge(
        facts: &CanonicalGenerationFacts,
        (source, target): (&SymbolInput, &SymbolInput),
        kind: EdgeKind,
    ) -> bool {
        facts.edges().iter().any(|edge| {
            edge.source_symbol_id == source.symbol_id
                && edge.target_symbol_id == target.symbol_id
                && edge.kind == kind
        })
    }

    fn jvm_binding_provenance<'facts>(
        facts: &'facts CanonicalGenerationFacts,
        reference_name: &str,
    ) -> &'facts str {
        facts
            .references()
            .iter()
            .find(|reference| reference.reference_name == reference_name)
            .map_or_else(
                || panic!("missing reference {reference_name}"),
                |reference| reference.resolution_provenance.as_str(),
            )
    }

    #[test]
    fn mybatis_bindings_link_packaged_mappers_parameters_and_template_ids() {
        let forward = jvm_binding_generation();
        let has_edge = |source: &SymbolInput, target: &SymbolInput, kind: EdgeKind| {
            jvm_binding_edge(&forward, (source, target), kind)
        };

        // Packaged mapper interface <-> XML namespace with the same package.
        let mapper = capability_symbol(
            &forward,
            "src/main/java/com/example/OrderMapper.java",
            "com.example::OrderMapper::findOrder",
        );
        let other_mapper = capability_symbol(
            &forward,
            "src/main/java/com/other/OrderMapper.java",
            "com.other::OrderMapper::findOrder",
        );
        let statement = capability_symbol(
            &forward,
            "src/main/resources/mapper/OrderMapper.xml",
            "OrderMapper::findOrder",
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == mapper.symbol_id
                && edge.target_symbol_id == statement.symbol_id
                && edge.kind == EdgeKind::References
                && edge.provenance == MYBATIS_BRIDGE_PROVENANCE
        }));
        assert!(
            !has_edge(other_mapper, statement, EdgeKind::References),
            "a mapper in another package must not bind to com.example.OrderMapper"
        );
        let unpackaged = capability_symbol(
            &forward,
            "src/main/java/OrderMapper.java",
            "OrderMapper::findOrder",
        );
        let confidence = |source: &SymbolInput| {
            forward
                .edges()
                .iter()
                .find(|edge| {
                    edge.source_symbol_id == source.symbol_id
                        && edge.target_symbol_id == statement.symbol_id
                })
                .map(|edge| edge.confidence)
        };
        assert_eq!(confidence(mapper), Some(EXACT_PROJECT_CONFIDENCE));
        assert_eq!(
            confidence(unpackaged),
            Some(FRAMEWORK_CONVENTION_CONFIDENCE),
            "package-erasing compatibility bindings are not exact"
        );
        // SqlSessionTemplate statement ids.
        let dao = capability_symbol(
            &forward,
            "src/main/java/com/example/order/dao/impl/OrderAttributeDaoImpl.java",
            "com.example.order.dao.impl::OrderAttributeDaoImpl::deleteByOrderId",
        );
        let delete = capability_symbol(
            &forward,
            "src/main/resources/mapper/OrderAttributeDaoMapper.xml",
            "OrderAttributeDaoMapper::deleteByOrderId",
        );
        assert!(has_edge(dao, delete, EdgeKind::References));
    }

    fn duplicate_mybatis_index(count: usize) -> ResolutionIndex {
        let source = r#"<mapper namespace="com.example.OrderMapper"><select id="findOrder">SELECT 1</select></mapper>"#;
        let limits = SourceLimits::new(TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("invalid limits: {error}"));
        let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
        let mut extractor = NativeExtractor::new(SourceLanguage::Xml)
            .unwrap_or_else(|error| panic!("missing XML grammar: {error}"));
        for ordinal in 0..count {
            let path = format!("mapper/OrderMapper{ordinal}.xml");
            let snapshot =
                cartograph_extract::SourceSnapshot::from_bytes(&path, source.as_bytes(), limits)
                    .unwrap_or_else(|error| panic!("invalid snapshot: {error}"));
            let file = extractor
                .extract(&snapshot)
                .unwrap_or_else(|error| panic!("XML extraction failed: {error}"));
            accumulator
                .push(file)
                .unwrap_or_else(|_| panic!("corpus exceeded budget"));
        }
        mybatis_test_resolution_index(&accumulator, TEST_GENERATION_BYTES)
    }

    fn mybatis_test_resolution_index(
        accumulator: &NativeFactAccumulator,
        maximum_bytes: u64,
    ) -> ResolutionIndex {
        let mut budget = ResolveBudget::new(0, maximum_bytes)
            .unwrap_or_else(|_| panic!("invalid resolve budget"));
        build_resolution_index(
            accumulator,
            ResolutionIndexContext {
                source_root: &test_source_root(),
                budget: &mut budget,
                cancelled: &mut || false,
            },
        )
        .unwrap_or_else(|_| panic!("index exceeded budget"))
    }

    fn mixed_mybatis_alias_index(counts: (usize, usize)) -> ResolutionIndex {
        const CORPUS_BYTES: u64 = 256 * 1024 * 1024;
        let limits = SourceLimits::new(TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("invalid limits: {error}"));
        let mut accumulator = NativeFactAccumulator::new(CORPUS_BYTES);
        for (language, count) in [
            (SourceLanguage::Java, counts.0),
            (SourceLanguage::Php, counts.1),
        ] {
            let mut extractor = NativeExtractor::new(language)
                .unwrap_or_else(|error| panic!("missing grammar: {error}"));
            for ordinal in 0..count {
                let (path, source) = mybatis_alias_fixture(language, ordinal);
                let snapshot = cartograph_extract::SourceSnapshot::from_bytes(
                    &path,
                    source.as_bytes(),
                    limits,
                )
                .unwrap_or_else(|error| panic!("invalid snapshot: {error}"));
                let file = extractor
                    .extract(&snapshot)
                    .unwrap_or_else(|error| panic!("extraction failed: {error}"));
                accumulator
                    .push(file)
                    .unwrap_or_else(|_| panic!("corpus exceeded budget"));
            }
        }
        mybatis_test_resolution_index(&accumulator, CORPUS_BYTES)
    }

    fn mybatis_alias_fixture(language: SourceLanguage, ordinal: usize) -> (String, String) {
        match language {
            SourceLanguage::Java => (
                format!("java/{ordinal}/OrderMapper.java"),
                "public class OrderMapper { public void findOrder() {} }".to_owned(),
            ),
            SourceLanguage::Php => (
                format!("php/OrderMapper{ordinal}.php"),
                format!(
                    "<?php namespace Ns{ordinal}; class OrderMapper {{ public static function findOrder() {{}} }}"
                ),
            ),
            _ => panic!("unsupported fixture language"),
        }
    }

    #[test]
    fn qualified_mybatis_zero_xml_alias_selection_is_linear_and_cancellable() {
        const JVM_COUNT: usize = 256;
        const PHP_COUNT: usize = 10_000;
        let index = mixed_mybatis_alias_index((JVM_COUNT, PHP_COUNT));
        let candidates = index
            .candidates
            .get("OrderMapper::findOrder")
            .unwrap_or_else(|| panic!("missing mapper bucket"))
            .as_slice();
        assert_eq!(candidates.len(), JVM_COUNT + PHP_COUNT);
        for cancel_after in [usize::MAX, 8] {
            let mut polls = 0;
            let mut facts = GenerationFacts::default();
            let mut budget = ResolveBudget::new(0, TEST_GENERATION_BYTES)
                .unwrap_or_else(|_| panic!("invalid resolve budget"));
            let result = append_mybatis_qualified_edges(
                ResolutionMutation {
                    index: &index,
                    facts: &mut facts,
                    budget: &mut budget,
                    cancelled: &mut || {
                        polls += 1;
                        polls == cancel_after
                    },
                },
                "OrderMapper::findOrder",
                candidates,
            );
            assert_eq!(result.is_err(), cancel_after == 8);
            assert!(polls <= candidates.len(), "selection work was {polls}");
            assert_eq!(polls, cancel_after.min(candidates.len()));
            assert_eq!(facts.edges, []);
            assert_eq!(budget.charged_bytes, 0);
        }
    }

    #[test]
    fn qualified_mybatis_xml_target_selection_obeys_its_allocation_budget() {
        let index = duplicate_mybatis_index(16);
        let candidates = index
            .candidates
            .get("OrderMapper::findOrder")
            .unwrap_or_else(|| panic!("missing statement bucket"))
            .as_slice();
        let mut budget = ResolveBudget::new(0, TEST_GENERATION_BYTES)
            .unwrap_or_else(|_| panic!("invalid resolve budget"));
        let targets = mybatis_xml_targets(
            (&index, "OrderMapper::findOrder", candidates),
            &mut budget,
            &mut || false,
        )
        .unwrap_or_else(|_| panic!("target selection failed"));
        assert_eq!(targets.len(), candidates.len());
        assert_eq!(
            budget.charged_bytes,
            usize_to_u64(targets.capacity())
                .saturating_mul(usize_to_u64(size_of::<&ResolutionCandidate>()))
        );
        let mut too_small =
            ResolveBudget::new(0, 0).unwrap_or_else(|_| panic!("invalid resolve budget"));
        assert!(
            mybatis_xml_targets(
                (&index, "OrderMapper::findOrder", candidates),
                &mut too_small,
                &mut || false
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_mybatis_mappers_cancel_without_emitting_edges() {
        let index = duplicate_mybatis_index(256);
        let candidates = index
            .candidates
            .get("OrderMapper::findOrder")
            .unwrap_or_else(|| panic!("missing statement bucket"))
            .as_slice();
        for packaged in [true, false] {
            let mut facts = GenerationFacts::default();
            let mut budget = ResolveBudget::new(0, TEST_GENERATION_BYTES)
                .unwrap_or_else(|_| panic!("invalid resolve budget"));
            let mut polls = 0;
            let mutation = ResolutionMutation {
                index: &index,
                facts: &mut facts,
                budget: &mut budget,
                cancelled: &mut || {
                    polls += 1;
                    polls == 8
                },
            };
            let result = if packaged {
                append_mybatis_packaged_edges(mutation, "OrderMapper::findOrder", candidates)
            } else {
                append_mybatis_qualified_edges(mutation, "OrderMapper::findOrder", candidates)
            };
            assert_matches!(result, Err(StageItemFailure));
            assert_eq!(polls, 8);
            assert_eq!(facts.edges, []);
        }
    }

    #[test]
    fn duplicate_mybatis_mapper_parent_lookups_are_file_scoped() {
        const COUNT: usize = 256;
        let index = duplicate_mybatis_index(COUNT);
        let candidates = index
            .candidates
            .get("OrderMapper::findOrder")
            .unwrap_or_else(|| panic!("missing statement bucket"))
            .as_slice();
        assert_eq!(candidates.len(), COUNT);
        let mut polls = 0;
        for statement in candidates {
            let name = mybatis_jvm_statement_name(&index, statement, &mut || {
                polls += 1;
                false
            })
            .unwrap_or_else(|_| panic!("name lookup failed"));
            assert_eq!(name.as_deref(), Some("com.example::OrderMapper::findOrder"));
        }
        assert_eq!(
            polls, COUNT,
            "each parent lookup examines only its file's namespace"
        );
        let mut facts = GenerationFacts::default();
        let mut budget = ResolveBudget::new(0, TEST_GENERATION_BYTES)
            .unwrap_or_else(|_| panic!("invalid resolve budget"));
        polls = 0;
        assert!(
            append_mybatis_packaged_edges(
                ResolutionMutation {
                    index: &index,
                    facts: &mut facts,
                    budget: &mut budget,
                    cancelled: &mut || {
                        polls += 1;
                        false
                    },
                },
                "OrderMapper::findOrder",
                candidates
            )
            .is_ok()
        );
        assert_eq!(polls, COUNT * 2);
        assert_eq!(facts.edges, []);
    }

    #[test]
    fn mybatis_parameters_prefer_exact_packages_and_keep_ambiguity() {
        let forward = jvm_binding_generation();
        let has_edge = |source: &SymbolInput, target: &SymbolInput, kind: EdgeKind| {
            jvm_binding_edge(&forward, (source, target), kind)
        };
        let statement = capability_symbol(
            &forward,
            "src/main/resources/mapper/OrderMapper.xml",
            "OrderMapper::findOrder",
        );
        let parameter = capability_symbol(
            &forward,
            "src/main/java/com/example/OrderMapper.java",
            "com.example::OrderMapper::findOrder::orderId",
        );
        assert!(has_edge(statement, parameter, EdgeKind::References));
        let other_parameter = capability_symbol(
            &forward,
            "src/main/java/com/other/OrderMapper.java",
            "com.other::OrderMapper::findOrder::orderId",
        );
        assert!(!has_edge(statement, other_parameter, EdgeKind::References));

        // An unpackaged Kotlin mapper keeps binding through the source-visible name.
        let kotlin_parameter = capability_symbol(
            &forward,
            "src/main/kotlin/UserMapper.kt",
            "UserMapper::findById::userId",
        );
        let user_statement = capability_symbol(
            &forward,
            "src/main/resources/mapper/UserMapper.xml",
            "UserMapper::findById",
        );
        assert!(has_edge(
            user_statement,
            kotlin_parameter,
            EdgeKind::References
        ));

        assert_eq!(
            jvm_binding_provenance(&forward, "OrderMapper::findOrder::orderId"),
            EXACT_PROJECT_PROVENANCE,
            "packaged mapper parameters bind by exact package-qualified identity"
        );
        assert_eq!(
            jvm_binding_provenance(&forward, "UserMapper::findById::userId"),
            FRAMEWORK_CONVENTION_PROVENANCE,
            "the unpackaged-mapper compatibility binding is not recorded as exact"
        );
        // Two packaged declarations are ambiguous; that ambiguity is kept
        // rather than falling back to the unpackaged compatibility match.
        let audit = forward
            .references()
            .iter()
            .find(|reference| reference.reference_name == "AuditMapper::record::entry")
            .unwrap_or_else(|| panic!("missing AuditMapper parameter reference"));
        assert!(
            audit.target_symbol_id.is_none(),
            "ambiguous packaged parameters must not resolve: {audit:?}"
        );
    }

    #[test]
    fn spring_and_kotlin_bindings_link_configuration_keys_and_receivers() {
        let forward = jvm_binding_generation();
        let has_edge = |source: &SymbolInput, target: &SymbolInput, kind: EdgeKind| {
            jvm_binding_edge(&forward, (source, target), kind)
        };

        // Spring configuration keys.
        let config = capability_symbol(
            &forward,
            "src/main/java/com/example/PaymentsAutoConfig.java",
            "com.example::PaymentsAutoConfig",
        );
        let enabled = capability_symbol(
            &forward,
            "config/application.properties",
            "feature.payments.enabled",
        );
        assert!(has_edge(config, enabled, EdgeKind::References));
        let field = capability_symbol(
            &forward,
            "src/main/java/com/example/PaymentsAutoConfig.java",
            "com.example::PaymentsAutoConfig::cacheTtl",
        );
        let ttl = capability_symbol(&forward, "config/application.properties", "app.cache.ttl");
        assert!(has_edge(field, ttl, EdgeKind::References));
        assert!(!has_edge(config, ttl, EdgeKind::References));

        // Kotlin extension functions are receiver-qualified and their bare
        // implicit-receiver calls keep resolving in the same and other files.
        let extension =
            capability_symbol(&forward, "src/main/kotlin/Ext.kt", "com.demo::Box::open");
        assert_eq!(extension.symbol_kind, SymbolKind::Function.as_str());
        let shout = capability_symbol(
            &forward,
            "src/main/kotlin/Ext.kt",
            "com.demo::String::shout",
        );
        let echo = capability_symbol(&forward, "src/main/kotlin/Ext.kt", "com.demo::String::echo");
        let loud = capability_symbol(
            &forward,
            "src/main/kotlin/Other.kt",
            "com.demo::String::loud",
        );
        assert!(has_edge(echo, shout, EdgeKind::Calls), "same-file call");
        assert!(has_edge(loud, shout, EdgeKind::Calls), "cross-file call");
    }

    #[test]
    fn kotlin_qualified_type_uses_and_extension_receivers_resolve() {
        fn source_path(qualified_name: &str) -> &'static str {
            if qualified_name.starts_with("r::") {
                "src/main/kotlin/r/Result.kt"
            } else {
                "src/main/kotlin/q/Take.kt"
            }
        }
        let forward = build_capability_generation(
            &[
                (
                    "src/main/kotlin/r/Result.kt",
                    "package r\nsealed class Result { class Success : Result() }\nfun handle(x: Result.Success) {}\n",
                ),
                (
                    "src/main/kotlin/q/Take.kt",
                    "package q\nimport r.Result\nclass Outer { class Nest }\nclass Box\nfun take(x: Result.Success) {}\nfun same(w: Outer.Nest, v: q.Outer) {}\nfun q.Box.peek() {}\n",
                ),
            ],
            false,
        );
        let has_type_of = |source: &str, target: &str| {
            let source = capability_symbol(&forward, source_path(source), source);
            let target = capability_symbol(&forward, source_path(target), target);
            jvm_binding_edge(&forward, (source, target), EdgeKind::TypeOf)
        };
        // Nested and sealed-subtype uses keep naming each written segment.
        assert!(has_type_of("r::handle", "r::Result"));
        assert!(has_type_of("q::take", "r::Result::Success"));
        assert!(has_type_of("q::same", "q::Outer"));
        // A written receiver path references its terminal type only.
        assert!(has_type_of("q::q.Box::peek", "q::Box"));
        assert!(
            forward
                .references()
                .iter()
                .all(|reference| reference.reference_name != "q.Box"),
            "receiver path stored as one unresolvable dotted type: {:?}",
            forward.references()
        );
    }

    #[test]
    fn framework_conventions_choose_one_unique_best_path_and_abstain_on_ties() {
        let preferred = [
            (
                "src/components/HomeView.ts",
                "export function HomeView(): void {}\n",
            ),
            (
                "src/generated/HomeView.ts",
                "export function HomeView(): void {}\n",
            ),
            (
                "src/screens/render.ts",
                "export function render(): void { HomeView(); }\n",
            ),
        ];
        let forward = build_capability_generation(&preferred, false);
        let reversed = build_capability_generation(&preferred, true);
        assert_eq!(forward.digest(), reversed.digest());
        let target = capability_symbol(&forward, "src/components/HomeView.ts", "HomeView");
        let reference = forward
            .references()
            .iter()
            .find(|reference| reference.reference_name == "HomeView")
            .unwrap_or_else(|| panic!("HomeView reference missing: {:?}", forward.references()));
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(
            reference.resolution_provenance,
            FRAMEWORK_CONVENTION_PROVENANCE
        );

        let tied = build_capability_generation(
            &[
                (
                    "packages/a/components/HomeView.ts",
                    "export function HomeView(): void {}\n",
                ),
                (
                    "packages/b/components/HomeView.ts",
                    "export function HomeView(): void {}\n",
                ),
                (
                    "src/screens/render.ts",
                    "export function render(): void { HomeView(); }\n",
                ),
            ],
            false,
        );
        let reference = tied
            .references()
            .iter()
            .find(|reference| reference.reference_name == "HomeView")
            .unwrap_or_else(|| panic!("tied HomeView reference missing: {:?}", tied.references()));
        assert!(reference.target_symbol_id.is_none());
        assert_eq!(reference.resolution_provenance, UNRESOLVED_PROVENANCE);
    }

    const NATIVE_FRAMEWORK_BRIDGE_FIXTURES: [(&str, &str); 8] = [
        (
            "src/native.ts",
            "import { NativeModules, TurboModuleRegistry } from 'react-native';\nimport { requireNativeModule } from 'expo-modules-core';\nNativeModules.Geolocation.getCurrentPosition();\nNativeModules.Scanner.startScan();\nconst Haptics = requireNativeModule('ExpoHaptics');\nHaptics.notificationAsync();\nNativeModules.DeviceInfo.getConstants();\nNativeModules.Geolocation.addListener('ignored');\nTurboModuleRegistry.getEnforcing<Spec>('DeviceInfo');\n",
        ),
        (
            "src/NativeDeviceInfo.ts",
            "interface Spec extends TurboModule {\n  getConstants(): { model: string };\n}\nexport default TurboModuleRegistry.getEnforcing<Spec>('DeviceInfo');\n",
        ),
        (
            "src/FooNativeComponent.ts",
            "import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent';\ninterface NativeProps { color?: string; }\nexport default codegenNativeComponent<NativeProps>('Foo');\n",
        ),
        (
            "ios/RCTGeolocation.m",
            "@implementation RCTGeolocation\nRCT_EXPORT_MODULE(Geolocation)\nRCT_EXPORT_METHOD(getCurrentPosition:(RCTResponseSenderBlock)callback) {}\nRCT_EXPORT_METHOD(addListener:(NSString *)name) {}\n@end\n",
        ),
        (
            "ios/RCTDeviceInfo.m",
            "@implementation RCTDeviceInfo\nRCT_EXPORT_MODULE(DeviceInfo)\n- (NSDictionary *)getConstants { return @{}; }\n@end\n",
        ),
        (
            "ios/RCTFooViewManager.m",
            "@interface RCTFooViewManager : RCTViewManager\n@end\n@implementation RCTFooViewManager\nRCT_EXPORT_VIEW_PROPERTY(color, NSString)\n@end\n",
        ),
        (
            "android/ScannerModule.kt",
            "class ScannerModule {\n  @ReactMethod\n  fun startScan() {}\n}\n",
        ),
        (
            "ios/HapticsModule.swift",
            "import ExpoModulesCore\npublic class HapticsModule: Module {\n  public func definition() -> ModuleDefinition {\n    Name(\"ExpoHaptics\")\n    AsyncFunction(\"notificationAsync\") { }\n  }\n}\n",
        ),
    ];

    fn assert_ambiguous_native_bridge_abstains() {
        let ambiguous = build_capability_generation(
            &[
                (
                    "src/native.ts",
                    "import { NativeModules } from 'react-native';\nNativeModules.Scanner.startScan();\n",
                ),
                (
                    "android/FirstScannerModule.kt",
                    "class FirstScannerModule { @ReactMethod fun startScan() {} }\n",
                ),
                (
                    "android/SecondScannerModule.kt",
                    "class SecondScannerModule { @ReactMethod fun startScan() {} }\n",
                ),
            ],
            false,
        );
        let start_scan = ambiguous
            .references()
            .iter()
            .filter(|reference| reference.reference_name == "startScan")
            .collect::<Vec<_>>();
        assert!(!start_scan.is_empty(), "missing ambiguous native call");
        assert!(
            start_scan
                .iter()
                .all(|reference| reference.target_symbol_id.is_none())
        );
        assert!(start_scan.iter().any(|reference| {
            reference.resolution_provenance == DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        }));
        assert!(
            start_scan
                .iter()
                .any(|reference| { reference.resolution_provenance == UNRESOLVED_PROVENANCE })
        );
    }

    #[test]
    fn native_framework_bridges_resolve_only_explicit_unique_exports() {
        let fixtures = NATIVE_FRAMEWORK_BRIDGE_FIXTURES;
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        assert_eq!(forward.references(), reversed.references());
        assert_eq!(forward.edges(), reversed.edges());

        NamedReferenceAssertion::new(&forward, "getCurrentPosition").targets(
            "ios/RCTGeolocation.m",
            "ios/RCTGeolocation.m::react-native-method::Geolocation::getCurrentPosition",
        );
        NamedReferenceAssertion::new(&forward, "startScan").targets(
            "android/ScannerModule.kt",
            "android/ScannerModule.kt::react-native-method::Scanner::startScan",
        );
        NamedReferenceAssertion::new(&forward, "notificationAsync").targets(
            "ios/HapticsModule.swift",
            "ios/HapticsModule.swift::expo-module-method::ExpoHaptics::notificationAsync",
        );
        NamedReferenceAssertion::new(&forward, "getConstants").targets(
            "src/NativeDeviceInfo.ts",
            "src/NativeDeviceInfo.ts::turbo-module-spec-method::DeviceInfo::getConstants",
        );
        assert!(
            forward
                .references()
                .iter()
                .filter(|reference| reference.reference_name == "addListener")
                .all(|reference| reference.target_symbol_id.is_none())
        );

        let module_spec = capability_symbol(
            &forward,
            "src/native.ts",
            "src/native.ts::native-module-spec::DeviceInfo",
        );
        let native_module = capability_symbol(
            &forward,
            "ios/RCTDeviceInfo.m",
            "ios/RCTDeviceInfo.m::react-native-module::DeviceInfo",
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == module_spec.symbol_id
                && edge.target_symbol_id == native_module.symbol_id
                && edge.provenance == NATIVE_MODULE_BRIDGE_PROVENANCE
        }));

        let turbo_method = capability_symbol(
            &forward,
            "src/NativeDeviceInfo.ts",
            "src/NativeDeviceInfo.ts::turbo-module-spec-method::DeviceInfo::getConstants",
        );
        let native_method = CapabilitySymbolQuery::new(&forward, "ios/RCTDeviceInfo.m")
            .matching("getConstants", "RCTDeviceInfo::");
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == turbo_method.symbol_id
                && edge.target_symbol_id == native_method.symbol_id
                && edge.provenance == TURBO_NATIVE_BRIDGE_PROVENANCE
        }));

        let fabric_spec = capability_symbol(
            &forward,
            "src/FooNativeComponent.ts",
            "src/FooNativeComponent.ts::fabric-component::Foo",
        );
        let native_view = capability_symbol(
            &forward,
            "ios/RCTFooViewManager.m",
            "ios/RCTFooViewManager.m::native-view-manager::Foo",
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == fabric_spec.symbol_id
                && edge.target_symbol_id == native_view.symbol_id
                && edge.provenance == FABRIC_NATIVE_BRIDGE_PROVENANCE
        }));

        assert_ambiguous_native_bridge_abstains();
    }

    #[test]
    fn swift_objc_bridge_honors_selectors_exposure_and_exact_native_precedence() {
        let fixtures = [
            (
                "ios/NativeDownloader.m",
                "#import <Foundation/Foundation.h>\n@implementation NativeDownloader\n- (void)downloadWithURL:(NSString *)url {}\n@end\n",
            ),
            (
                "ios/SwiftCaller.swift",
                "import Foundation\nfunc invoke(_ downloader: NativeDownloader) { downloader.download(url: \"safe\") }\n",
            ),
            (
                "ios/SwiftPlayer.swift",
                "import Foundation\nclass SwiftPlayer {\n  @objc func play(song: String) {}\n  @nonobjc func internalOnly() {}\n}\n",
            ),
            (
                "ios/ObjcCaller.m",
                "#import <Foundation/Foundation.h>\n@implementation ObjcCaller\n- (void)invoke:(SwiftPlayer *)player {\n  [player playWithSong:@\"safe\"];\n  [player internalOnly];\n}\n@end\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let objc_download = CapabilitySymbolQuery::new(&forward, "ios/NativeDownloader.m")
            .matching("download", "::objc-swift-method::");
        let swift_call =
            capability_reference_in_file(&forward, "ios/SwiftCaller.swift", "download");
        assert_eq!(
            swift_call.target_symbol_id.as_ref(),
            Some(&objc_download.symbol_id)
        );
        assert_eq!(swift_call.resolution_provenance, APPLE_BRIDGE_PROVENANCE);

        let swift_play = CapabilitySymbolQuery::new(&forward, "ios/SwiftPlayer.swift")
            .matching("play", "::swift-objc-method::");
        let objc_call = capability_reference_in_file(&forward, "ios/ObjcCaller.m", "playWithSong:");
        assert_eq!(
            objc_call.target_symbol_id.as_ref(),
            Some(&swift_play.symbol_id)
        );
        assert_eq!(objc_call.resolution_provenance, APPLE_BRIDGE_PROVENANCE);
        assert!(
            forward
                .references()
                .iter()
                .filter(|reference| reference.reference_name.ends_with("internalOnly"))
                .all(|reference| reference.target_symbol_id.is_none())
        );

        let exact = build_capability_generation(
            &[
                (
                    "ios/SwiftPlayer.swift",
                    "class SwiftPlayer { @objc func play(song: String) {} }\n",
                ),
                (
                    "ios/NativePlayer.m",
                    "@implementation NativePlayer\n- (void)playWithSong:(NSString *)song {}\n@end\n",
                ),
                (
                    "ios/Caller.m",
                    "#import <Foundation/Foundation.h>\n@implementation Caller\n- (void)run:(id)player { [player playWithSong:@\"safe\"]; }\n@end\n",
                ),
            ],
            false,
        );
        let exact_call = capability_reference_in_file(&exact, "ios/Caller.m", "playWithSong:");
        let exact_target = exact_call
            .target_symbol_id
            .as_ref()
            .and_then(|target| {
                exact
                    .symbols()
                    .iter()
                    .find(|symbol| &symbol.symbol_id == target)
            })
            .unwrap_or_else(|| panic!("missing exact ObjC target: {:?}", exact.references()));
        let exact_target_file = exact
            .files()
            .iter()
            .find(|file| file.file_id == exact_target.file_id)
            .unwrap_or_else(|| panic!("missing exact ObjC target file"));
        assert_eq!(exact_target_file.normalized_path, "ios/NativePlayer.m");
        assert_ne!(exact_call.resolution_provenance, APPLE_BRIDGE_PROVENANCE);
    }

    #[test]
    fn objc_keyword_selectors_imports_and_heritage_resolve_to_native_declarations() {
        let fixtures = [
            (
                "ios/Player.h",
                "#import <Foundation/Foundation.h>\n@interface Player : Base\n- (void)playWithSong:(NSString *)song volume:(int)volume;\n@end\n",
            ),
            (
                "ios/Player.m",
                "#import <Foundation/Foundation.h>\n#import \"Player.h\"\n@implementation Player\n- (void)playWithSong:(NSString *)song volume:(int)volume {}\n@end\n",
            ),
            (
                "ios/Base.h",
                "#import <Foundation/Foundation.h>\n@interface Base : NSObject\n@end\n",
            ),
            ("ios/Base.m", "@implementation Base\n@end\n"),
            (
                "ios/Caller.m",
                "#import <UIKit/UIKit.h>\n#import \"Player.h\"\n@implementation Caller\n- (void)run:(Player *)player {\n  [player playWithSong:@\"safe\" volume:1];\n  [player playWithSong:@\"safe\"];\n}\n@end\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let definition = CapabilitySymbolQuery::new(&forward, "ios/Player.m")
            .of_kind("Player::playWithSong:volume:", SymbolKind::Method);
        let keyword_send =
            capability_reference_in_file(&forward, "ios/Caller.m", "player.playWithSong:volume:");
        assert_eq!(
            keyword_send.target_symbol_id.as_ref(),
            Some(&definition.symbol_id),
            "a full-selector send resolves to the implementation, not the header"
        );
        assert_eq!(keyword_send.resolution_provenance, EXACT_PROJECT_PROVENANCE);
        let partial_send = forward
            .references()
            .iter()
            .find(|reference| reference.reference_name == "player.playWithSong:")
            .unwrap_or_else(|| panic!("missing partial send: {:?}", forward.references()));
        assert!(
            partial_send.target_symbol_id.is_none(),
            "a different selector must not resolve by its first keyword"
        );

        let base_definition =
            CapabilitySymbolQuery::new(&forward, "ios/Base.m").of_kind("Base", SymbolKind::Class);
        let header_player = CapabilitySymbolQuery::new(&forward, "ios/Player.h")
            .of_kind("Player", SymbolKind::Class);
        let extends = CapabilityReferenceQuery::new(&forward, header_player)
            .named("Base", ReferenceKind::Extends);
        assert_eq!(
            extends.target_symbol_id.as_ref(),
            Some(&base_definition.symbol_id)
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == header_player.symbol_id
                && edge.target_symbol_id == base_definition.symbol_id
                && edge.kind == EdgeKind::Extends
        }));

        let import = capability_reference_in_file(&forward, "ios/Player.m", "Player.h");
        assert_eq!(import.resolution_provenance, QUOTED_INCLUDE_PROVENANCE);
        assert!(import.target_symbol_id.is_some());
    }

    #[test]
    fn swift_objc_bridge_prefers_the_implementation_alias_over_its_interface_declaration() {
        let fixtures = [
            (
                "ios/Downloader.m",
                "#import <Foundation/Foundation.h>\n@interface Downloader : NSObject\n- (void)downloadWithURL:(NSString *)url;\n@end\n@implementation Downloader\n- (void)downloadWithURL:(NSString *)url {}\n@end\n",
            ),
            (
                "ios/Caller.swift",
                "import Foundation\nfunc fetch(_ downloader: Downloader) { downloader.download(url: \"safe\") }\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        let aliases = forward
            .symbols()
            .iter()
            .filter(|symbol| {
                symbol
                    .qualified_name
                    .ends_with("::objc-swift-method::downloadWithURL:::download")
                    || symbol
                        .qualified_name
                        .contains("::objc-swift-method::downloadWithURL:::download#")
            })
            .collect::<Vec<_>>();
        assert_eq!(aliases.len(), 2, "one alias per declaration and definition");
        let call = capability_reference_in_file(&forward, "ios/Caller.swift", "download");
        let target = call
            .target_symbol_id
            .as_ref()
            .and_then(|target| aliases.iter().find(|alias| &alias.symbol_id == target))
            .unwrap_or_else(|| panic!("bridge abstained: {call:?}"));
        assert!(!target.declaration_only);
        assert_eq!(call.resolution_provenance, APPLE_BRIDGE_PROVENANCE);
    }

    /// Competing Swift declarations per name: one in its conventional directory.
    const SWIFT_CONVENTION_FIXTURES: [(&str, &str); 15] = [
        (
            "Sources/TableViewCells/UserCell.swift",
            "class UserCell {}\n",
        ),
        ("Sources/Generated/UserCell.swift", "class UserCell {}\n"),
        (
            "Sources/Protocols/ProfileDelegate.swift",
            "protocol ProfileDelegate: AnyObject {}\n",
        ),
        (
            "Sources/Legacy/ProfileDelegate.swift",
            "class ProfileDelegate {}\n",
        ),
        ("Sources/UI/AvatarView.swift", "class AvatarView {}\n"),
        (
            "Sources/Generated/AvatarView.swift",
            "class AvatarView {}\n",
        ),
        (
            "Sources/App/Controllers/UserController.swift",
            "struct UserController {}\n",
        ),
        (
            "Sources/App/Other/UserController.swift",
            "struct UserController {}\n",
        ),
        (
            "Sources/App/Middleware/AuthMiddleware.swift",
            "struct AuthMiddleware {}\n",
        ),
        (
            "Sources/App/Other/AuthMiddleware.swift",
            "struct AuthMiddleware {}\n",
        ),
        (
            "Sources/App/Database/Account.swift",
            "final class Account {}\n",
        ),
        (
            "Sources/App/Other/Account.swift",
            "final class Account {}\n",
        ),
        ("Sources/Cells/LeftCell.swift", "class LeftCell {}\n"),
        ("Sources/Views/LeftCell.swift", "class LeftCell {}\n"),
        (
            "Sources/App/Screen.swift",
            "class Screen: ProfileDelegate {\n  let cell: UserCell\n  let left: LeftCell\n  let avatar: AvatarView\n  func boot() { UserController(); AuthMiddleware(); Account() }\n}\n",
        ),
    ];

    #[test]
    fn swift_uikit_and_vapor_conventions_prefer_conventional_directories_and_kinds() {
        let fixtures = SWIFT_CONVENTION_FIXTURES;
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        for (name, path, qualified_name) in [
            (
                "UserCell",
                "Sources/TableViewCells/UserCell.swift",
                "UserCell",
            ),
            (
                "ProfileDelegate",
                "Sources/Protocols/ProfileDelegate.swift",
                "ProfileDelegate",
            ),
            ("AvatarView", "Sources/UI/AvatarView.swift", "AvatarView"),
            (
                "UserController",
                "Sources/App/Controllers/UserController.swift",
                "UserController",
            ),
            (
                "AuthMiddleware",
                "Sources/App/Middleware/AuthMiddleware.swift",
                "AuthMiddleware",
            ),
            ("Account", "Sources/App/Database/Account.swift", "Account"),
        ] {
            let target = capability_symbol(&forward, path, qualified_name);
            let reference = forward
                .references()
                .iter()
                .find(|reference| reference.reference_name == name)
                .unwrap_or_else(|| panic!("missing {name}: {:?}", forward.references()));
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&target.symbol_id),
                "{name} did not prefer {path}"
            );
            assert_eq!(
                reference.resolution_provenance,
                FRAMEWORK_CONVENTION_PROVENANCE
            );
        }
        let tied = forward
            .references()
            .iter()
            .find(|reference| reference.reference_name == "LeftCell")
            .unwrap_or_else(|| panic!("missing LeftCell: {:?}", forward.references()));
        assert!(
            tied.target_symbol_id.is_none(),
            "two conventional cell directories must abstain"
        );
    }

    #[test]
    fn objc_selector_resolution_abstains_on_unary_and_shared_selectors_and_categories_do_not_define()
     {
        let fixtures = [
            (
                "ios/Left.m",
                "@implementation Left\n- (void)play:(int)speed {}\n- (void)stop {}\n@end\n",
            ),
            (
                "ios/Right.m",
                "@implementation Right\n- (void)play:(int)speed {}\n@end\n",
            ),
            ("ios/Shape.m", "@implementation Shape\n@end\n"),
            (
                "ios/Shape+Extras.m",
                "@implementation Shape (Extras)\n- (void)extra {}\n@end\n",
            ),
            (
                "ios/Square.m",
                "#import <Foundation/Foundation.h>\n@interface Square : Shape\n@end\n@implementation Square\n- (void)run:(Left *)left {\n  [left play:1];\n  [left stop];\n  [left act:2];\n}\n- (void)notify:(id)delegate {\n  [delegate feedDidFinish:1];\n  [delegate feedDidFail:2];\n}\n@end\n",
            ),
            (
                "ios/Worker.m",
                "#import <Foundation/Foundation.h>\n@protocol Loading\n- (void)load:(int)value;\n@end\n@implementation Worker\n- (void)load:(int)value {}\n- (void)run {\n  [self load:1];\n}\n@end\n",
            ),
            (
                "ios/FeedDelegate.h",
                "@protocol FeedDelegate\n- (void)feedDidFinish:(int)count;\n- (void)feedDidFail:(int)count;\n@end\n@protocol RetryDelegate\n- (void)feedDidFail:(int)count;\n@end\n",
            ),
            (
                "ios/Declared.h",
                "@interface Declared : NSObject\n- (void)act:(int)value;\n@end\n",
            ),
            (
                "ios/Unrelated.m",
                "@implementation Unrelated\n- (void)act:(int)value {}\n@end\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        for name in [
            "left.play:",
            "left.stop",
            "left.act:",
            "delegate.feedDidFail:",
        ] {
            let reference = capability_reference_in_file(&forward, "ios/Square.m", name);
            assert!(
                reference.target_symbol_id.is_none(),
                "{name} must abstain: {reference:?}"
            );
        }
        // A protocol requirement never competes with the one class that
        // implements the selector, and is itself the target only when no
        // class declares the selector and one protocol alone requires it.
        for (path, name, target) in [
            ("ios/Worker.m", "load:", "Worker::load:"),
            (
                "ios/Square.m",
                "delegate.feedDidFinish:",
                "FeedDelegate::feedDidFinish:",
            ),
        ] {
            let reference = capability_reference_in_file(&forward, path, name);
            let target = forward
                .symbols()
                .iter()
                .find(|symbol| {
                    symbol.qualified_name == target
                        && symbol.symbol_kind == SymbolKind::Method.as_str()
                })
                .unwrap_or_else(|| panic!("missing {target}: {:?}", forward.symbols()));
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&target.symbol_id),
                "{name}: {reference:?}"
            );
        }
        let shape =
            CapabilitySymbolQuery::new(&forward, "ios/Shape.m").of_kind("Shape", SymbolKind::Class);
        let category = CapabilitySymbolQuery::new(&forward, "ios/Shape+Extras.m")
            .of_kind("Shape", SymbolKind::Class);
        assert!(category.declaration_only, "a category augments its class");
        let square = CapabilitySymbolQuery::new(&forward, "ios/Square.m")
            .of_kind("Square", SymbolKind::Class);
        let extends =
            CapabilityReferenceQuery::new(&forward, square).named("Shape", ReferenceKind::Extends);
        assert_eq!(extends.target_symbol_id.as_ref(), Some(&shape.symbol_id));
    }

    #[test]
    fn swift_unqualified_calls_resolve_to_the_enclosing_types_methods_only() {
        let fixtures = [
            (
                "Sources/Feed.swift",
                "struct Feed {\n  func helper() {}\n  func run() {\n    helper()\n    run()\n  }\n}\nstruct Other {\n  func helper() {}\n  func go() { helper() }\n}\nextension Feed {\n  func later() { helper() }\n}\nfunc outer() {\n  func inner() {}\n  inner()\n}\n",
            ),
            (
                "Sources/Elsewhere.swift",
                "struct Elsewhere {\n  func helper() {}\n}\n",
            ),
            (
                "Sources/Bridged.swift",
                "@objcMembers\nclass Bridged: NSObject {\n  func recur() { recur() }\n}\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        for (caller, callee, target) in [
            ("Feed::run", "helper", "Feed::helper"),
            ("Other::go", "helper", "Other::helper"),
            ("Feed::later", "helper", "Feed::helper"),
            ("outer", "inner", "outer::inner"),
        ] {
            let caller = capability_symbol(&forward, "Sources/Feed.swift", caller);
            let target = capability_symbol(&forward, "Sources/Feed.swift", target);
            let reference =
                CapabilityReferenceQuery::new(&forward, caller).named(callee, ReferenceKind::Calls);
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&target.symbol_id),
                "{} -> {callee}",
                caller.qualified_name
            );
        }
        let run = capability_symbol(&forward, "Sources/Feed.swift", "Feed::run");
        let recursion =
            CapabilityReferenceQuery::new(&forward, run).named("run", ReferenceKind::Calls);
        assert_eq!(recursion.target_symbol_id.as_ref(), Some(&run.symbol_id));
        assert_eq!(recursion.resolution_provenance, EXACT_LEXICAL_PROVENANCE);
        let bridged = capability_symbol(&forward, "Sources/Bridged.swift", "Bridged::recur");
        let bridged_recursion =
            CapabilityReferenceQuery::new(&forward, bridged).named("recur", ReferenceKind::Calls);
        assert_eq!(
            bridged_recursion.target_symbol_id.as_ref(),
            Some(&bridged.symbol_id),
            "unexpected recursive binding: {bridged_recursion:?}"
        );
        assert_eq!(
            bridged_recursion.resolution_provenance,
            EXACT_LEXICAL_PROVENANCE
        );
    }

    #[test]
    fn turbo_module_specs_bridge_to_keyword_selector_implementations() {
        let fixtures = [
            (
                "src/NativeCalculator.ts",
                "interface Spec extends TurboModule {\n  multiply(a: number, b: number): number;\n}\nexport default TurboModuleRegistry.getEnforcing<Spec>('Calculator');\n",
            ),
            (
                "ios/RCTCalculator.m",
                "#import <React/RCTBridgeModule.h>\n@interface RCTCalculator : NSObject\n- (NSNumber *)multiply:(double)a b:(double)b;\n@end\n@implementation RCTCalculator\n- (NSNumber *)multiply:(double)a b:(double)b { return nil; }\n@end\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        let spec = capability_symbol(
            &forward,
            "src/NativeCalculator.ts",
            "src/NativeCalculator.ts::turbo-module-spec-method::Calculator::multiply",
        );
        let implementation = forward
            .symbols()
            .iter()
            .find(|symbol| {
                symbol.qualified_name == "RCTCalculator::multiply:b:" && !symbol.declaration_only
            })
            .unwrap_or_else(|| panic!("missing native implementation: {:?}", forward.symbols()));
        let bridges = forward
            .edges()
            .iter()
            .filter(|edge| {
                edge.source_symbol_id == spec.symbol_id
                    && edge.provenance == TURBO_NATIVE_BRIDGE_PROVENANCE
            })
            .collect::<Vec<_>>();
        assert_eq!(bridges.len(), 1, "{bridges:?}");
        assert_eq!(bridges[0].target_symbol_id, implementation.symbol_id);
    }

    #[test]
    fn swift_and_objc_framework_imports_never_hide_project_declarations() {
        let fixtures = [
            ("Sources/Api.swift", "public func fetchAll() {}\n"),
            (
                "Sources/Screen.swift",
                "import Foundation\nimport UIKit\nfunc load() { fetchAll() }\n",
            ),
            ("ios/Util.m", "void LogEvent(int code) {}\n"),
            (
                "ios/Screen.m",
                "#import <UIKit/UIKit.h>\n@implementation Screen\n- (void)show { LogEvent(1); }\n@end\n",
            ),
            ("src/util.c", "void log_event(int code) {}\n"),
            (
                "src/main.c",
                "#include <stdio.h>\nint main(void) { log_event(1); return 0; }\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        for (caller_path, name, target_path) in [
            ("Sources/Screen.swift", "fetchAll", "Sources/Api.swift"),
            ("ios/Screen.m", "LogEvent", "ios/Util.m"),
        ] {
            let target = capability_symbol(&forward, target_path, name);
            let reference = capability_reference_in_file(&forward, caller_path, name);
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&target.symbol_id),
                "{name}: {reference:?}"
            );
            assert_eq!(reference.resolution_provenance, EXACT_PROJECT_PROVENANCE);
        }
        let c_call = capability_reference_in_file(&forward, "src/main.c", "log_event");
        assert!(
            c_call.target_symbol_id.is_none(),
            "a C system include keeps vetoing project guesses: {c_call:?}"
        );
    }

    #[test]
    fn half_typed_objc_and_swift_declarations_never_fail_the_generation() {
        let fixtures = [
            ("ios/Empty.m", "@implementation\n@end\n"),
            ("ios/Interface.m", "@interface\n@end\n@protocol\n@end\n"),
            (
                "ios/Half.m",
                "@implementation Half\n- (void) {}\n- (void)kept {}\n@end\n",
            ),
            (
                "Sources/Broken.swift",
                "func () {}\nstruct S {\n  var : Int\n  func kept() {}\n}\nenum E {\n  case\n}\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        capability_symbol(&forward, "ios/Half.m", "Half::kept");
        capability_symbol(&forward, "Sources/Broken.swift", "S::kept");
        assert!(
            forward
                .symbols()
                .iter()
                .all(|symbol| !symbol.qualified_name.ends_with("::")),
            "{:?}",
            forward.symbols()
        );
    }

    #[test]
    fn swift_backtick_escaped_declarations_resolve_from_their_uses() {
        let fixtures = [(
            "Sources/Keywords.swift",
            "struct `Type` {\n  func `default`() {}\n  func run() { `default`() }\n}\nfunc `repeat`() {}\nfunc use(_ value: `Type`) {\n  `repeat`()\n}\n",
        )];
        let forward = build_capability_generation(&fixtures, false);
        for (caller, name, kind, target) in [
            ("use", "repeat", ReferenceKind::Calls, "repeat"),
            ("use", "Type", ReferenceKind::TypeOf, "Type"),
            (
                "Type::run",
                "default",
                ReferenceKind::Calls,
                "Type::default",
            ),
        ] {
            let caller = capability_symbol(&forward, "Sources/Keywords.swift", caller);
            let target = capability_symbol(&forward, "Sources/Keywords.swift", target);
            let reference = CapabilityReferenceQuery::new(&forward, caller).named(name, kind);
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&target.symbol_id),
                "{name}: {reference:?}"
            );
        }
    }

    #[test]
    fn drupal_service_tags_form_one_deterministic_cross_module_hub() {
        let fixtures = [
            (
                "modules/provider/provider.services.yml",
                "services:\n  demo.provider:\n    class: Drupal\\provider\\DemoProvider\n    tags:\n      - { name: demo.handlers }\n",
            ),
            (
                "modules/consumer/consumer.services.yml",
                "services:\n  demo.consumer:\n    class: Drupal\\consumer\\DemoConsumer\n    arguments:\n      - !tagged_iterator demo.handlers\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        assert_eq!(forward.edges(), reversed.edges());

        let provider = capability_symbol(
            &forward,
            "modules/provider/provider.services.yml",
            "modules/provider/provider.services.yml::drupal-service::demo.provider",
        );
        let consumer = capability_symbol(
            &forward,
            "modules/consumer/consumer.services.yml",
            "modules/consumer/consumer.services.yml::drupal-service::demo.consumer",
        );
        let provider_edge = forward
            .edges()
            .iter()
            .find(|edge| {
                edge.source_symbol_id == provider.symbol_id
                    && edge.provenance == DRUPAL_TAG_PROVIDES_PROVENANCE
            })
            .unwrap_or_else(|| panic!("missing Drupal provider edge: {:?}", forward.edges()));
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == consumer.symbol_id
                && edge.target_symbol_id == provider_edge.target_symbol_id
                && edge.provenance == DRUPAL_TAG_CONSUMES_PROVENANCE
        }));
    }

    #[test]
    fn manifest_workspaces_link_exact_members_respect_glob_depth_and_cargo_excludes() {
        let fixtures = [
            (
                "package.json",
                r#"{"name":"@acme/root","workspaces":["packages/*"],"dependencies":{"@acme/core":"workspace:*"}}"#,
            ),
            ("packages/core/package.json", r#"{"name":"@acme/core"}"#),
            (
                "packages/core/nested/package.json",
                r#"{"name":"@acme/nested"}"#,
            ),
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n",
            ),
            (
                "crates/core/Cargo.toml",
                "[package]\nname = \"core-crate\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/skip/Cargo.toml",
                "[package]\nname = \"skip-crate\"\nversion = \"0.1.0\"\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let npm_workspace = capability_symbol(
            &forward,
            "package.json",
            "package.json::manifest-workspace-member-npm::.::pattern::packages/*",
        );
        let npm_core = capability_symbol(
            &forward,
            "packages/core/package.json",
            "packages/core/package.json::manifest-package-npm::@acme/core::manifest-dir::packages/core",
        );
        let npm_nested = capability_symbol(
            &forward,
            "packages/core/nested/package.json",
            "packages/core/nested/package.json::manifest-package-npm::@acme/nested::manifest-dir::packages/core/nested",
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == npm_workspace.symbol_id
                && edge.target_symbol_id == npm_core.symbol_id
                && edge.provenance == MANIFEST_WORKSPACE_PROVENANCE
        }));
        assert!(forward.edges().iter().all(|edge| {
            edge.source_symbol_id != npm_workspace.symbol_id
                || edge.target_symbol_id != npm_nested.symbol_id
        }));
        NamedReferenceAssertion::new(&forward, "@acme/core").targets(
            "packages/core/package.json",
            "packages/core/package.json::manifest-package-npm::@acme/core::manifest-dir::packages/core",
        );

        let cargo_workspace = capability_symbol(
            &forward,
            "Cargo.toml",
            "Cargo.toml::manifest-workspace-member-cargo::.::pattern::crates/*",
        );
        let cargo_core = capability_symbol(
            &forward,
            "crates/core/Cargo.toml",
            "crates/core/Cargo.toml::manifest-package-cargo::core-crate::manifest-dir::crates/core",
        );
        let cargo_skip = capability_symbol(
            &forward,
            "crates/skip/Cargo.toml",
            "crates/skip/Cargo.toml::manifest-package-cargo::skip-crate::manifest-dir::crates/skip",
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == cargo_workspace.symbol_id
                && edge.target_symbol_id == cargo_core.symbol_id
                && edge.provenance == MANIFEST_WORKSPACE_PROVENANCE
        }));
        assert!(forward.edges().iter().all(|edge| {
            edge.source_symbol_id != cargo_workspace.symbol_id
                || edge.target_symbol_id != cargo_skip.symbol_id
        }));
    }

    #[test]
    fn component_framework_aliases_default_imports_stores_and_local_preference_resolve() {
        let fixtures = [
            ("src/lib/Card.svelte", "<article>Card</article>\n"),
            ("src/lib/Button.svelte", "<button>Library</button>\n"),
            ("src/routes/Button.svelte", "<button>Local</button>\n"),
            ("src/stores.ts", "export const count = 0;\n"),
            (
                "src/routes/+page.svelte",
                "<script>\nimport Card from '$lib/Card';\nimport { goto } from '$app/navigation';\nimport { count } from '../stores';\n</script>\n<Card/><Button/>{$count}\n",
            ),
            (
                "src/components/Card.vue",
                "<template><article>Card</article></template>\n",
            ),
            (
                "components/Button.vue",
                "<template><button>Library</button></template>\n",
            ),
            (
                "pages/Button.vue",
                "<template><button>Local</button></template>\n",
            ),
            (
                "pages/index.vue",
                "<script setup>\nimport Card from '@/components/Card';\n</script>\n<template><Card/><Button/></template>\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let svelte_card = capability_symbol(&forward, "src/lib/Card.svelte", "Card");
        let local_svelte_button = capability_symbol(&forward, "src/routes/Button.svelte", "Button");
        let count = capability_symbol(&forward, "src/stores.ts", "count");
        let module_reference =
            capability_reference_in_file(&forward, "src/routes/+page.svelte", "$lib/Card");
        assert_eq!(
            module_reference.target_symbol_id.as_ref(),
            Some(&svelte_card.symbol_id)
        );
        assert_eq!(
            module_reference.resolution_provenance,
            "native-conventional-alias"
        );
        let card_reference =
            capability_reference_in_file(&forward, "src/routes/+page.svelte", "Card");
        assert_eq!(
            card_reference.target_symbol_id.as_ref(),
            Some(&svelte_card.symbol_id)
        );
        assert_eq!(
            card_reference.resolution_provenance,
            "native-conventional-alias"
        );
        let button_reference =
            capability_reference_in_file(&forward, "src/routes/+page.svelte", "Button");
        assert_eq!(
            button_reference.target_symbol_id.as_ref(),
            Some(&local_svelte_button.symbol_id)
        );
        let store_reference =
            capability_reference_in_file(&forward, "src/routes/+page.svelte", "$count");
        assert_eq!(
            store_reference.target_symbol_id.as_ref(),
            Some(&count.symbol_id)
        );

        let vue_card = capability_symbol(&forward, "src/components/Card.vue", "Card");
        let local_vue_button = capability_symbol(&forward, "pages/Button.vue", "Button");
        let vue_module =
            capability_reference_in_file(&forward, "pages/index.vue", "@/components/Card");
        assert_eq!(
            vue_module.target_symbol_id.as_ref(),
            Some(&vue_card.symbol_id)
        );
        assert_eq!(
            vue_module.resolution_provenance,
            "native-conventional-alias"
        );
        let vue_card_reference = capability_reference_in_file(&forward, "pages/index.vue", "Card");
        assert_eq!(
            vue_card_reference.target_symbol_id.as_ref(),
            Some(&vue_card.symbol_id)
        );
        let vue_button_reference =
            capability_reference_in_file(&forward, "pages/index.vue", "Button");
        assert_eq!(
            vue_button_reference.target_symbol_id.as_ref(),
            Some(&local_vue_button.symbol_id)
        );
    }

    const COMPONENT_RESOLUTION_FIXTURES: [(&str, &str); 5] = [
        (
            "src/lib/format.ts",
            "export function formatOrder() {}\nexport default function loadOrder() {}\n",
        ),
        (
            "src/components/OrderPanel.vue",
            "<script setup lang=\"ts\">\nimport loadOrder, { formatOrder } from '../lib/format';\nconst order = loadOrder();\n</script>\n<template><p>{{ formatOrder(order) }}</p><p>{{ missingHelper() }}</p></template>\n",
        ),
        (
            "src/components/OrderBadge.svelte",
            "<script lang=\"ts\">\nimport { formatOrder } from '../lib/format';\n</script>\n<span>{formatOrder()}</span>\n",
        ),
        ("src/layouts/Layout.astro", "<main><slot /></main>\n"),
        (
            "src/pages/orders.astro",
            "---\nimport Layout from '../layouts/Layout.astro';\nimport { formatOrder } from '../lib/format';\n---\n<Layout><h1>{formatOrder()}</h1></Layout>\n",
        ),
    ];

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn component_corpus_is_identical_for_every_worker_count() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create component fixture: {error}"));
        for (path, source) in COMPONENT_RESOLUTION_FIXTURES {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("component fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
        let serial = build(directory.path(), SERIAL_WORKERS).await;
        for workers in [2, 4, 8, 16] {
            let parallel = build(directory.path(), workers).await;
            assert_eq!(
                serial.facts().digest(),
                parallel.facts().digest(),
                "{workers} workers"
            );
        }
        let library = capability_file_symbol(serial.facts(), "src/lib/format.ts");
        let module = capability_resolved_reference(
            serial.facts(),
            "src/pages/orders.astro",
            ("../lib/format", ReferenceKind::Imports),
        );
        assert_eq!(module.target_symbol_id.as_ref(), Some(&library.symbol_id));
    }

    #[test]
    fn component_scripts_and_template_expressions_resolve_through_their_imports() {
        let forward = build_capability_generation(&COMPONENT_RESOLUTION_FIXTURES, false);
        let reversed = build_capability_generation(&COMPONENT_RESOLUTION_FIXTURES, true);
        assert_eq!(forward.digest(), reversed.digest());

        let library = capability_file_symbol(&forward, "src/lib/format.ts");
        let format_order = capability_symbol(&forward, "src/lib/format.ts", "formatOrder");
        let load_order = capability_symbol(&forward, "src/lib/format.ts", "loadOrder");
        let layout = capability_symbol(&forward, "src/layouts/Layout.astro", "Layout");
        for path in [
            "src/components/OrderPanel.vue",
            "src/components/OrderBadge.svelte",
            "src/pages/orders.astro",
        ] {
            let module = capability_resolved_reference(
                &forward,
                path,
                ("../lib/format", ReferenceKind::Imports),
            );
            assert_eq!(
                module.target_symbol_id.as_ref(),
                Some(&library.symbol_id),
                "{path}"
            );
            assert_eq!(
                module.resolution_provenance, MODULE_IMPORT_PROVENANCE,
                "{path}"
            );
            let call = capability_resolved_reference(
                &forward,
                path,
                ("formatOrder", ReferenceKind::Calls),
            );
            assert_eq!(
                call.target_symbol_id.as_ref(),
                Some(&format_order.symbol_id),
                "{path}"
            );
            assert_eq!(
                call.resolution_provenance, IMPORT_BINDING_PROVENANCE,
                "{path}"
            );
        }
        let default_call = capability_resolved_reference(
            &forward,
            "src/components/OrderPanel.vue",
            ("loadOrder", ReferenceKind::Calls),
        );
        assert_eq!(
            default_call.target_symbol_id.as_ref(),
            Some(&load_order.symbol_id)
        );
        let unimported = capability_resolved_reference(
            &forward,
            "src/components/OrderPanel.vue",
            ("missingHelper", ReferenceKind::Calls),
        );
        assert_eq!(unimported.target_symbol_id, None);
        let component_use = capability_resolved_reference(
            &forward,
            "src/pages/orders.astro",
            ("Layout", ReferenceKind::References),
        );
        assert_eq!(
            component_use.target_symbol_id.as_ref(),
            Some(&layout.symbol_id)
        );
    }

    #[test]
    fn component_module_scope_exports_defaults_and_schemas_resolve() {
        let fixtures = [
            ("src/lib/helpers.ts", "export function assist() {}\n"),
            (
                "src/lib/Helpers.svelte",
                "<script context=\"module\">\nexport * as helpers from './helpers';\nfunction tool() {}\nexport { tool };\n</script>\n",
            ),
            (
                "src/components/Options.vue",
                "<script>\nexport default {\n  name: 'Options',\n  methods: { save() {} },\n};\n</script>\n",
            ),
            (
                "src/pages/use.vue",
                "<script setup>\nimport Options from '../components/Options.vue';\n</script>\n<template><Options /></template>\n",
            ),
            (
                "src/components/Form.svelte",
                "<script lang=\"ts\">\nimport { z } from 'zod';\nconst UserSchema = z.object({ name: z.string() });\nfunction check() { return UserSchema.shape.name; }\n</script>\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let options = capability_symbol(&forward, "src/components/Options.vue", "Options");
        let options_use = capability_resolved_reference(
            &forward,
            "src/pages/use.vue",
            ("Options", ReferenceKind::References),
        );
        assert_eq!(
            options_use.target_symbol_id.as_ref(),
            Some(&options.symbol_id),
            "the options-API default export must not compete with the component"
        );
        let field = capability_symbol(&forward, "src/components/Form.svelte", "UserSchema::name");
        let consumer = capability_resolved_reference(
            &forward,
            "src/components/Form.svelte",
            ("name", ReferenceKind::References),
        );
        assert_eq!(consumer.target_symbol_id.as_ref(), Some(&field.symbol_id));
        let tool = capability_symbol(&forward, "src/lib/Helpers.svelte", "tool");
        assert!(tool.export.exported);
        capability_symbol(&forward, "src/lib/Helpers.svelte", "helpers");
    }

    fn capability_resolved_reference<'facts>(
        facts: &'facts CanonicalGenerationFacts,
        path: &str,
        (name, kind): (&str, ReferenceKind),
    ) -> &'facts ReferenceInput {
        let file_id = &capability_file_symbol(facts, path).file_id;
        facts
            .references()
            .iter()
            .find(|reference| {
                &reference.file_id == file_id
                    && reference.reference_name == name
                    && reference.reference_kind == kind.as_str()
            })
            .unwrap_or_else(|| panic!("missing {kind:?} {name} in {path}"))
    }

    #[test]
    fn php_framework_routes_resolve_controller_methods_and_classes_without_losing_source_labels() {
        let fixtures = [
            (
                "app/Http/Controllers/OrderController.php",
                "<?php\nnamespace App\\Http\\Controllers;\nclass OrderController { public function index() {} public function store() {} public function show() {} }\n",
            ),
            (
                "routes/web.php",
                "<?php\nuse App\\Http\\Controllers\\OrderController;\nRoute::get('/orders', [OrderController::class, 'index']);\nRoute::post('/orders', 'OrderController@store');\nRoute::resource('orders', OrderController::class);\n",
            ),
            (
                "config/routes.yaml",
                "orders_show:\n  path: /orders/{id}\n  controller: App\\Http\\Controllers\\OrderController::show\n  methods: [GET]\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let controller = capability_symbol(
            &forward,
            "app/Http/Controllers/OrderController.php",
            "App\\Http\\Controllers::OrderController",
        );
        let index =
            CapabilitySymbolQuery::new(&forward, "app/Http/Controllers/OrderController.php")
                .matching("index", "OrderController");
        let store =
            CapabilitySymbolQuery::new(&forward, "app/Http/Controllers/OrderController.php")
                .matching("store", "OrderController");
        let show = CapabilitySymbolQuery::new(&forward, "app/Http/Controllers/OrderController.php")
            .matching("show", "OrderController");
        for (path, reference_name, target) in [
            ("routes/web.php", "index", index),
            ("routes/web.php", "OrderController@store", store),
            ("routes/web.php", "OrderController", controller),
            (
                "config/routes.yaml",
                "App\\Http\\Controllers\\OrderController::show",
                show,
            ),
        ] {
            let file_id = forward
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .map_or_else(
                    || panic!("missing PHP route fixture {path}"),
                    |file| &file.file_id,
                );
            assert!(
                forward.references().iter().any(|reference| {
                    &reference.file_id == file_id
                        && reference.reference_name == reference_name
                        && reference.target_symbol_id.as_ref() == Some(&target.symbol_id)
                }),
                "missing resolved PHP route {path}:{reference_name} -> {}; refs={:?}",
                target.qualified_name,
                forward
                    .references()
                    .iter()
                    .filter(|reference| &reference.file_id == file_id)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn php_framework_route_resolution_abstains_when_controller_identity_is_ambiguous() {
        let fixtures = [
            (
                "app/Http/Controllers/Admin/OrderController.php",
                "<?php\nnamespace App\\Http\\Controllers\\Admin;\nclass OrderController { public function index() {} }\n",
            ),
            (
                "app/Http/Controllers/Partner/OrderController.php",
                "<?php\nnamespace App\\Http\\Controllers\\Partner;\nclass OrderController { public function index() {} }\n",
            ),
            (
                "routes/web.php",
                "<?php\nRoute::get('/orders', [OrderController::class, 'index']);\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let reference = capability_reference_in_file(&forward, "routes/web.php", "index");
        assert_eq!(reference.reference_name, "index");
        assert!(reference.target_symbol_id.is_none());
        assert_eq!(reference.resolution_provenance, UNRESOLVED_PROVENANCE);
    }

    #[test]
    fn schema_contracts_and_dynamic_dispatch_resolve_to_searchable_native_graph_facts() {
        let fixtures = [
            (
                "src/contracts.ts",
                "import { z } from 'zod';\nexport const UserSchema = z.object({ role: z.enum(['admin', 'viewer']) });\nexport type User = z.infer<typeof UserSchema>;\nexport function startHandler() {}\nconst HANDLERS = { start: startHandler };\nexport function dispatch(kind: string) { HANDLERS[kind]?.(); return UserSchema.shape.role; }\n",
            ),
            (
                "models.py",
                "from pydantic import BaseModel\nclass Account(BaseModel):\n    id: str\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let schema = CapabilitySymbolQuery::new(&forward, "src/contracts.ts")
            .of_kind("UserSchema", SymbolKind::Struct);
        assert_eq!(schema.symbol_kind, SymbolKind::Struct.as_str());
        let role = CapabilitySymbolQuery::new(&forward, "src/contracts.ts")
            .of_kind("UserSchema::role", SymbolKind::Field);
        let type_reference = forward
            .references()
            .iter()
            .find(|reference| {
                reference.reference_name == "UserSchema"
                    && reference.reference_kind == ReferenceKind::TypeOf.as_str()
            })
            .unwrap_or_else(|| panic!("missing Zod infer reference: {:?}", forward.references()));
        assert_eq!(
            type_reference.target_symbol_id.as_ref(),
            Some(&schema.symbol_id)
        );
        let field_reference = forward
            .references()
            .iter()
            .find(|reference| {
                reference.reference_name == "role"
                    && reference.reference_kind == ReferenceKind::References.as_str()
            })
            .unwrap_or_else(|| panic!("missing Zod field reference: {:?}", forward.references()));
        assert_eq!(
            field_reference.target_symbol_id.as_ref(),
            Some(&role.symbol_id),
            "Zod field reference did not resolve: {:?}",
            forward.references()
        );

        let handler = capability_symbol(&forward, "src/contracts.ts", "startHandler");
        let source_file = capability_file_symbol(&forward, "src/contracts.ts");
        let dispatch_reference = forward
            .references()
            .iter()
            .find(|reference| {
                reference.reference_name == "startHandler"
                    && reference.resolution_provenance == DYNAMIC_DISPATCH_PROVENANCE
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing dynamic dispatch reference: {:?}",
                    forward.references()
                )
            });
        assert_eq!(
            dispatch_reference.owner_symbol_id.as_ref(),
            Some(&source_file.symbol_id)
        );
        assert_eq!(
            dispatch_reference.target_symbol_id.as_ref(),
            Some(&handler.symbol_id)
        );
        assert_confidence(dispatch_reference.confidence, DYNAMIC_DISPATCH_CONFIDENCE);
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == source_file.symbol_id
                && edge.target_symbol_id == handler.symbol_id
                && edge.kind == EdgeKind::Calls
                && edge.provenance == DYNAMIC_DISPATCH_PROVENANCE
        }));

        assert_eq!(
            CapabilitySymbolQuery::new(&forward, "models.py")
                .of_kind("Account", SymbolKind::Struct)
                .symbol_kind,
            SymbolKind::Struct.as_str()
        );
        assert_eq!(
            CapabilitySymbolQuery::new(&forward, "models.py")
                .of_kind("Account::id", SymbolKind::Field)
                .symbol_kind,
            SymbolKind::Field.as_str()
        );
    }

    #[test]
    fn value_and_type_only_imports_retain_external_consumers() {
        let fixtures = [
            (
                "src/service.ts",
                "import { z } from 'zod';\nexport const RuntimeSchema = z.object({ value: z.string() });\nexport type RuntimeSchema = z.infer<typeof RuntimeSchema>;\nexport const RuntimeConfig = { mode: 'safe' } as const;\n",
            ),
            (
                "src/build.ts",
                "import { RuntimeSchema } from './service';\nexport function parseRuntime(value: unknown) { return RuntimeSchema.safeParse(value); }\n",
            ),
            (
                "src/model.ts",
                "export type PublicRecord = Readonly<{ id: string }>;\n",
            ),
            (
                "src/consumer.ts",
                "import type { PublicRecord } from './model';\nexport function readId(value: PublicRecord): string { return value.id; }\n",
            ),
            (
                "src/config-consumer.ts",
                "import type { RuntimeConfig } from './service';\nexport type RuntimeConfigShape = typeof RuntimeConfig;\n",
            ),
        ];
        let facts = build_capability_generation(&fixtures, false);
        for (source_path, target_path, name) in [
            ("src/build.ts", "src/service.ts", "RuntimeSchema"),
            ("src/consumer.ts", "src/model.ts", "PublicRecord"),
            ("src/config-consumer.ts", "src/service.ts", "RuntimeConfig"),
        ] {
            let source_file = facts
                .files()
                .iter()
                .find(|file| file.normalized_path == source_path)
                .unwrap_or_else(|| panic!("missing source file {source_path}"));
            let target_file = facts
                .files()
                .iter()
                .find(|file| file.normalized_path == target_path)
                .unwrap_or_else(|| panic!("missing target file {target_path}"));
            let targets = facts
                .symbols()
                .iter()
                .filter(|symbol| {
                    symbol.file_id == target_file.file_id && symbol.qualified_name == name
                })
                .map(|symbol| &symbol.symbol_id)
                .collect::<Vec<_>>();
            assert!(!targets.is_empty(), "missing target {target_path}::{name}");
            assert!(
                facts.edges().iter().any(|edge| {
                    targets.contains(&&edge.target_symbol_id)
                        && facts.symbols().iter().any(|symbol| {
                            symbol.symbol_id == edge.source_symbol_id
                                && symbol.file_id == source_file.file_id
                        })
                }),
                "missing external consumer edge {source_path} -> {target_path}::{name}; references={:?}",
                facts.references()
            );
        }
        let runtime_config = facts
            .symbols()
            .iter()
            .find(|symbol| {
                symbol.qualified_name == "RuntimeConfig"
                    && symbol.symbol_kind == SymbolKind::Constant.as_str()
            })
            .unwrap_or_else(|| panic!("missing RuntimeConfig value declaration"));
        let runtime_config_shape =
            capability_symbol(&facts, "src/config-consumer.ts", "RuntimeConfigShape");
        assert!(facts.references().iter().any(|reference| {
            reference.owner_symbol_id.as_ref() == Some(&runtime_config_shape.symbol_id)
                && reference.target_symbol_id.as_ref() == Some(&runtime_config.symbol_id)
                && reference.reference_name == "RuntimeConfig"
                && reference.reference_kind == ReferenceKind::TypeOf.as_str()
                && reference.resolution_provenance == IMPORT_BINDING_PROVENANCE
        }));
    }

    #[test]
    fn python_intrinsics_and_receiver_calls_remain_explicit_non_project_boundaries() {
        let fixtures = [(
            "src/normalize.py",
            "def normalize(values):\n    for index, value in enumerate(values):\n        text = str(value).strip()\n        values.append(text)\n    if len(values) == 0:\n        raise RuntimeError('empty')\n    return missing_project_symbol(values)\n",
        )];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());
        let normalize = capability_symbol(&forward, "src/normalize.py", "normalize");

        for name in ["enumerate", "str", "len", "RuntimeError"] {
            let reference = forward
                .references()
                .iter()
                .find(|reference| {
                    reference.owner_symbol_id.as_ref() == Some(&normalize.symbol_id)
                        && reference.reference_name == name
                        && reference.reference_kind == ReferenceKind::Calls.as_str()
                })
                .unwrap_or_else(|| panic!("missing Python intrinsic reference {name}"));
            assert!(reference.target_symbol_id.is_none(), "{name}");
            assert_eq!(
                reference.resolution_provenance, PYTHON_INTRINSIC_UNRESOLVED_PROVENANCE,
                "{name}"
            );
        }
        for member in ["append", "strip"] {
            let reference = forward
                .references()
                .iter()
                .find(|reference| {
                    reference.owner_symbol_id.as_ref() == Some(&normalize.symbol_id)
                        && reference.reference_kind == ReferenceKind::Calls.as_str()
                        && reference
                            .reference_name
                            .rsplit('.')
                            .next()
                            .is_some_and(|name| name == member)
                })
                .unwrap_or_else(|| panic!("missing Python receiver reference {member}"));
            assert!(reference.target_symbol_id.is_none(), "{member}");
            assert_eq!(
                reference.resolution_provenance, DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
                "{member}"
            );
        }
        let missing = forward
            .references()
            .iter()
            .find(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&normalize.symbol_id)
                    && reference.reference_name == "missing_project_symbol"
            })
            .unwrap_or_else(|| panic!("missing unresolved project-symbol control"));
        assert!(missing.target_symbol_id.is_none());
        assert_eq!(missing.resolution_provenance, UNRESOLVED_PROVENANCE);
    }

    #[test]
    fn static_javascript_value_references_resolve_to_traversable_handler_edges() {
        let fixtures = [(
            "src/wire.tsx",
            "export function saveHandler() {}\nexport function cancelHandler() {}\nfunction configure(value: unknown) { return value; }\nexport function wire(pretty: boolean) {\n  configure(saveHandler);\n  const options = { onSave: saveHandler, cancelHandler };\n  const steps = [saveHandler, cancelHandler];\n  const form = <form onSubmit={saveHandler} />;\n  (pretty ? saveHandler : cancelHandler)();\n  return { options, steps, form };\n}\n",
        )];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let wire = capability_symbol(&forward, "src/wire.tsx", "wire");
        let save = capability_symbol(&forward, "src/wire.tsx", "saveHandler");
        let cancel = capability_symbol(&forward, "src/wire.tsx", "cancelHandler");
        for (name, target, minimum_sites) in
            [("saveHandler", save, 5_usize), ("cancelHandler", cancel, 3)]
        {
            let references = forward
                .references()
                .iter()
                .filter(|reference| {
                    reference.reference_name == name
                        && reference.reference_kind == ReferenceKind::References.as_str()
                        && reference.target_symbol_id.as_ref() == Some(&target.symbol_id)
                        && reference.resolution_provenance == EXACT_SAME_FILE_PROVENANCE
                })
                .collect::<Vec<_>>();
            assert!(
                references.len() >= minimum_sites,
                "missing resolved value references for {name}: {:?}",
                forward.references()
            );
            for reference in references {
                let owner = reference
                    .owner_symbol_id
                    .as_ref()
                    .unwrap_or_else(|| panic!("value reference {name} lost its lexical owner"));
                assert!(
                    owner == &wire.symbol_id
                        || forward.edges().iter().any(|edge| {
                            edge.source_symbol_id == wire.symbol_id
                                && &edge.target_symbol_id == owner
                                && edge.kind == EdgeKind::Contains
                        }),
                    "value reference {name} escaped wire's lexical graph"
                );
                assert!(forward.edges().iter().any(|edge| {
                    &edge.source_symbol_id == owner
                        && edge.target_symbol_id == target.symbol_id
                        && edge.kind == EdgeKind::References
                        && edge.provenance == EXACT_SAME_FILE_PROVENANCE
                }));
            }
        }
    }

    #[test]
    fn dynamic_import_members_and_destructuring_resolve_to_exported_symbols() {
        let fixtures = [
            (
                "src/target.ts",
                "export function foo() {}\nexport function bar() {}\nexport function qux() {}\nexport function direct() {}\n",
            ),
            ("src/types.ts", "export interface Widget { id: string }\n"),
            (
                "src/panel.tsx",
                "export default function Panel() { return null; }\n",
            ),
            (
                "src/consumer.ts",
                "import { lazy } from 'react';\nexport const Panel = lazy(() => import('./panel'));\nexport async function load() { const { foo, bar: alias } = await import('./target.js'); const module = await import('./target.js'); module.qux(); const inline = import('./target.js').direct; return [foo, alias, inline]; }\ntype Loaded = import('./types.js').Widget;\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        for (path, name) in [
            ("src/target.ts", "foo"),
            ("src/target.ts", "bar"),
            ("src/target.ts", "qux"),
            ("src/target.ts", "direct"),
            ("src/types.ts", "Widget"),
            ("src/panel.tsx", "Panel"),
        ] {
            let target = capability_symbol(&forward, path, name);
            assert!(
                forward.references().iter().any(|reference| {
                    reference.target_symbol_id.as_ref() == Some(&target.symbol_id)
                        && reference.resolution_provenance == IMPORT_BINDING_PROVENANCE
                }),
                "missing dynamic import resolution for {path}:{name}: {:?}",
                forward.references()
            );
            assert!(
                forward.edges().iter().any(|edge| {
                    edge.target_symbol_id == target.symbol_id
                        && matches!(
                            edge.kind,
                            EdgeKind::References
                                | EdgeKind::FieldAccess
                                | EdgeKind::Calls
                                | EdgeKind::TypeOf
                        )
                        && edge.provenance == IMPORT_BINDING_PROVENANCE
                }),
                "missing dynamic import edge for {path}:{name}: {:?}",
                forward
                    .edges()
                    .iter()
                    .filter(|edge| edge.target_symbol_id == target.symbol_id)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn wildcard_reexports_expand_transitively_with_namespace_default_and_ambiguity_rules() {
        let fixtures = [
            (
                "src/target.ts",
                "export function foo() {}\nexport function bar() {}\nexport default function targetDefault() {}\n",
            ),
            ("src/local.ts", "export function localFoo() {}\n"),
            (
                "src/types.ts",
                "export interface Widget { id: string }\nexport default class TypeDefault {}\n",
            ),
            (
                "src/barrel.ts",
                "export { localFoo as foo } from './local.js';\nexport * from './target.js';\nexport * as types from './types.js';\n",
            ),
            ("src/deep.ts", "export * from './barrel.js';\n"),
            ("src/a.ts", "export function duplicate() {}\n"),
            ("src/b.ts", "export function duplicate() {}\n"),
            (
                "src/ambiguous.ts",
                "export * from './a.js';\nexport * from './b.js';\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let barrel_file = capability_file_symbol(&forward, "src/barrel.ts");
        let deep_file = capability_file_symbol(&forward, "src/deep.ts");
        let ambiguous_file = capability_file_symbol(&forward, "src/ambiguous.ts");
        let target_foo = capability_symbol(&forward, "src/target.ts", "foo");
        let target_bar = capability_symbol(&forward, "src/target.ts", "bar");
        let target_default = capability_symbol(&forward, "src/target.ts", "targetDefault");
        let namespace = capability_symbol(&forward, "src/barrel.ts", "types");
        let widget = capability_symbol(&forward, "src/types.ts", "Widget");
        let type_default = capability_symbol(&forward, "src/types.ts", "TypeDefault");

        assert!(!forward.edges().iter().any(|edge| {
            edge.source_symbol_id == barrel_file.symbol_id
                && edge.target_symbol_id == target_foo.symbol_id
                && edge.provenance == RE_EXPORT_ALL_PROVENANCE
        }));
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == barrel_file.symbol_id
                && edge.target_symbol_id == target_bar.symbol_id
                && edge.kind == EdgeKind::Exports
                && edge.provenance == RE_EXPORT_ALL_PROVENANCE
        }));
        assert!(!forward.edges().iter().any(|edge| {
            edge.source_symbol_id == barrel_file.symbol_id
                && edge.target_symbol_id == target_default.symbol_id
                && edge.provenance == RE_EXPORT_ALL_PROVENANCE
        }));
        for target in [widget, type_default] {
            assert!(forward.edges().iter().any(|edge| {
                edge.source_symbol_id == namespace.symbol_id
                    && edge.target_symbol_id == target.symbol_id
                    && edge.kind == EdgeKind::Exports
                    && edge.provenance == RE_EXPORT_NAMESPACE_PROVENANCE
            }));
        }
        for target in [target_bar, namespace] {
            assert!(forward.edges().iter().any(|edge| {
                edge.source_symbol_id == deep_file.symbol_id
                    && edge.target_symbol_id == target.symbol_id
                    && edge.kind == EdgeKind::Exports
                    && edge.provenance == RE_EXPORT_ALL_PROVENANCE
            }));
        }
        for duplicate in [
            capability_symbol(&forward, "src/a.ts", "duplicate"),
            capability_symbol(&forward, "src/b.ts", "duplicate"),
        ] {
            assert!(!forward.edges().iter().any(|edge| {
                edge.source_symbol_id == ambiguous_file.symbol_id
                    && edge.target_symbol_id == duplicate.symbol_id
                    && edge.provenance == RE_EXPORT_ALL_PROVENANCE
            }));
        }
    }

    const TEST_SUBJECT_FIXTURES: [(&str, &str); 13] = [
        ("src/math.ts", "export function add() { return 2; }\n"),
        (
            "src/math.test.ts",
            "import { add } from './math.js'; test('adds', () => add());\n",
        ),
        (
            "tests/feature.ts",
            "import { add } from '../src/math.js'; test('feature', () => add());\n",
        ),
        ("src/sync/index.ts", "export function sync() {}\n"),
        (
            "__tests__/sync.test.ts",
            "import { sync } from '../src/sync/index.js'; test('syncs', sync);\n",
        ),
        ("src/worker.py", "def work():\n    return True\n"),
        (
            "tests/test_worker.py",
            "from src.worker import work\ndef test_work():\n    assert work()\n",
        ),
        ("src/parser.rs", "pub fn parse() {}\n"),
        (
            "src/parser_test.rs",
            "use crate::parser::parse;\n#[test]\nfn parses() { parse(); }\n",
        ),
        (
            "crates/core/src/lib.rs",
            "pub fn run() {}\n#[cfg(test)]\nmod tests { #[test] fn runs() { super::run(); } }\n",
        ),
        (
            "crates/core/tests/api.rs",
            "use core::run;\n#[test]\nfn api_runs() { run(); }\n",
        ),
        (
            "src/main/java/com/acme/Service.java",
            "package com.acme; public class Service { public void run() {} }\n",
        ),
        (
            "src/test/java/com/acme/ServiceTest.java",
            "package com.acme; public class ServiceTest { void verifies() { new Service().run(); } }\n",
        ),
    ];

    const EXPECTED_TEST_SUBJECT_EDGES: [(&str, &str, &str, f32); 7] = [
        (
            "src/math.test.ts",
            "src/math.ts",
            TEST_CONVENTION_PROVENANCE,
            TEST_CONVENTION_CONFIDENCE,
        ),
        (
            "tests/feature.ts",
            "src/math.ts",
            TEST_IMPORT_PROVENANCE,
            TEST_IMPORT_CONFIDENCE,
        ),
        (
            "__tests__/sync.test.ts",
            "src/sync/index.ts",
            TEST_CONVENTION_PROVENANCE,
            TEST_CONVENTION_CONFIDENCE,
        ),
        (
            "tests/test_worker.py",
            "src/worker.py",
            TEST_CONVENTION_PROVENANCE,
            TEST_CONVENTION_CONFIDENCE,
        ),
        (
            "src/parser_test.rs",
            "src/parser.rs",
            TEST_CONVENTION_PROVENANCE,
            TEST_CONVENTION_CONFIDENCE,
        ),
        (
            "crates/core/tests/api.rs",
            "crates/core/src/lib.rs",
            RUST_INTEGRATION_TEST_PROVENANCE,
            EXTRACTED_EDGE_CONFIDENCE,
        ),
        (
            "src/test/java/com/acme/ServiceTest.java",
            "src/main/java/com/acme/Service.java",
            TEST_CONVENTION_PROVENANCE,
            TEST_CONVENTION_CONFIDENCE,
        ),
    ];

    #[test]
    fn test_subject_edges_cover_conventions_imports_jvm_and_rust_native_patterns() {
        let fixtures = TEST_SUBJECT_FIXTURES;
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        for (test_path, subject_path, provenance, confidence) in EXPECTED_TEST_SUBJECT_EDGES {
            let test = capability_file_symbol(&forward, test_path);
            let subject = capability_file_symbol(&forward, subject_path);
            assert!(
                forward.edges().iter().any(|edge| {
                    edge.source_symbol_id == test.symbol_id
                        && edge.target_symbol_id == subject.symbol_id
                        && edge.kind == EdgeKind::Tests
                        && edge.provenance == provenance
                        && confidence_matches(edge.confidence, confidence)
                }),
                "missing test edge {test_path} -> {subject_path}: {:?}",
                forward
                    .edges()
                    .iter()
                    .filter(|edge| edge.kind == EdgeKind::Tests)
                    .collect::<Vec<_>>()
            );
        }

        let inline = capability_file_symbol(&forward, "crates/core/src/lib.rs");
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == inline.symbol_id
                && edge.target_symbol_id == inline.symbol_id
                && edge.kind == EdgeKind::Tests
                && edge.provenance == RUST_INLINE_TEST_PROVENANCE
        }));
    }

    #[test]
    fn nested_functions_are_first_class_without_usage_threshold_promotion() {
        let fixtures = [(
            "src/nested.ts",
            "export function outer() { function nested(value: number) { return helper(value); } return nested(3); }\nfunction helper(value: number) { return value; }\n",
        )];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let outer = capability_symbol(&forward, "src/nested.ts", "outer");
        let nested = capability_symbol(&forward, "src/nested.ts", "outer::nested");
        let helper = capability_symbol(&forward, "src/nested.ts", "helper");
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == outer.symbol_id
                && edge.target_symbol_id == nested.symbol_id
                && edge.kind == EdgeKind::Contains
        }));
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == outer.symbol_id
                && edge.target_symbol_id == nested.symbol_id
                && edge.kind == EdgeKind::Calls
        }));
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == nested.symbol_id
                && edge.target_symbol_id == helper.symbol_id
                && edge.kind == EdgeKind::Calls
        }));
        assert!(forward.documents().iter().any(|document| {
            document.symbol_id() == Some(&nested.symbol_id)
                && document.qualified_name() == "outer::nested"
        }));
    }

    #[test]
    fn mega_file_nested_functions_are_complete_bounded_and_deterministic() {
        const NESTED_FUNCTIONS: usize = 2_400;
        let mut source = String::from("export function outer(value: number) {\n");
        for index in 0..NESTED_FUNCTIONS {
            writeln!(
                source,
                "  function nested_{index:04}(input: number) {{ return input + value; }}"
            )
            .unwrap_or_else(|error| panic!("mega fixture rendering failed: {error}"));
        }
        source.push_str("  return nested_2399(value);\n}\n");
        source.push_str("/* generated mega-file padding\n");
        source.push_str(&"x".repeat(512 * 1_024));
        source.push_str("\n*/\n");
        let fixtures = [("src/mega.ts", source.as_str())];
        let first = build_capability_generation_with_budget(
            &fixtures,
            false,
            (false, MEGA_TEST_GENERATION_BYTES),
        );
        let second = build_capability_generation_with_budget(
            &fixtures,
            false,
            (false, MEGA_TEST_GENERATION_BYTES),
        );
        assert_eq!(first.digest(), second.digest());

        let nested = first
            .symbols()
            .iter()
            .filter(|symbol| {
                symbol.symbol_kind == SymbolKind::Function.as_str()
                    && symbol.qualified_name.starts_with("outer::nested_")
            })
            .collect::<Vec<_>>();
        assert_eq!(nested.len(), NESTED_FUNCTIONS);
        let nested_ids = nested
            .iter()
            .map(|symbol| symbol.symbol_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(nested_ids.len(), NESTED_FUNCTIONS);
        let outer = capability_symbol(&first, "src/mega.ts", "outer");
        let contained = first
            .edges()
            .iter()
            .filter(|edge| {
                edge.source_symbol_id == outer.symbol_id
                    && edge.kind == EdgeKind::Contains
                    && nested_ids.contains(edge.target_symbol_id.as_str())
            })
            .count();
        assert_eq!(contained, NESTED_FUNCTIONS);
        assert_eq!(
            first
                .documents()
                .iter()
                .filter(|document| {
                    document
                        .symbol_id()
                        .is_some_and(|symbol_id| nested_ids.contains(symbol_id.as_str()))
                })
                .count(),
            NESTED_FUNCTIONS
        );
        assert!((512 * 1_024..1024 * 1024).contains(&source.len()));
        let parameters = first
            .symbols()
            .iter()
            .filter(|symbol| symbol.symbol_kind == SymbolKind::Parameter.as_str())
            .count();
        assert_eq!(parameters, NESTED_FUNCTIONS + 1);
        assert!(
            first.symbols().len() <= (NESTED_FUNCTIONS * 2) + 3,
            "mega fixture retained {} symbols",
            first.symbols().len()
        );
        assert!(
            first.edges().len() <= (NESTED_FUNCTIONS * 5) + 8,
            "mega fixture retained {} edges",
            first.edges().len()
        );
    }

    #[test]
    fn go_structural_interfaces_use_signatures_and_cross_file_receiver_ownership() {
        let fixtures = [
            (
                "api/reader.go",
                "package api\ntype Reader interface { Read(payload []byte) (int, error); Close() error }\ntype Resettable interface { reset() }\ntype Empty interface{}\n",
            ),
            (
                "impl/readers.go",
                "package impl\ntype FileReader struct{}\nfunc (reader *FileReader) Read(buffer []byte) (int, error) { return 0, nil }\nfunc (reader *FileReader) Close() error { return nil }\ntype WrongReader struct{}\nfunc (reader *WrongReader) Read(value string) (int, error) { return 0, nil }\nfunc (reader *WrongReader) Close() error { return nil }\ntype ForeignReset struct{}\nfunc (value *ForeignReset) reset() {}\n",
            ),
            ("worker/types.go", "package worker\ntype Worker struct{}\n"),
            (
                "worker/methods.go",
                "package worker\nfunc (worker *Worker) Run(task string) error { return nil }\n",
            ),
            (
                "contracts/runner.go",
                "package contracts\ntype Runner interface { Run(name string) error }\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let reader = CapabilitySymbolQuery::new(&forward, "api/reader.go")
            .of_kind("Reader", SymbolKind::Interface);
        let resettable = CapabilitySymbolQuery::new(&forward, "api/reader.go")
            .of_kind("Resettable", SymbolKind::Interface);
        let empty = CapabilitySymbolQuery::new(&forward, "api/reader.go")
            .of_kind("Empty", SymbolKind::Interface);
        let file_reader = CapabilitySymbolQuery::new(&forward, "impl/readers.go")
            .of_kind("FileReader", SymbolKind::Struct);
        let wrong_reader = CapabilitySymbolQuery::new(&forward, "impl/readers.go")
            .of_kind("WrongReader", SymbolKind::Struct);
        let foreign_reset = CapabilitySymbolQuery::new(&forward, "impl/readers.go")
            .of_kind("ForeignReset", SymbolKind::Struct);
        let worker = CapabilitySymbolQuery::new(&forward, "worker/types.go")
            .of_kind("Worker", SymbolKind::Struct);
        let run = CapabilitySymbolQuery::new(&forward, "worker/methods.go")
            .of_kind("Worker::Run", SymbolKind::Method);
        let runner = CapabilitySymbolQuery::new(&forward, "contracts/runner.go")
            .of_kind("Runner", SymbolKind::Interface);

        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == file_reader.symbol_id
                && edge.target_symbol_id == reader.symbol_id
                && edge.kind == EdgeKind::Implements
                && edge.provenance == GO_IMPLEMENTS_PROVENANCE
                && edge.confidence == GO_STRUCTURAL_CONFIDENCE
        }));
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == worker.symbol_id
                && edge.target_symbol_id == run.symbol_id
                && edge.kind == EdgeKind::Contains
                && edge.provenance == GO_RECEIVER_OWNERSHIP_PROVENANCE
        }));
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == worker.symbol_id
                && edge.target_symbol_id == runner.symbol_id
                && edge.kind == EdgeKind::Implements
                && edge.provenance == GO_IMPLEMENTS_PROVENANCE
        }));
        for (source, target) in [
            (wrong_reader, reader),
            (foreign_reset, resettable),
            (file_reader, empty),
            (wrong_reader, empty),
        ] {
            assert!(
                !forward.edges().iter().any(|edge| {
                    edge.source_symbol_id == source.symbol_id
                        && edge.target_symbol_id == target.symbol_id
                        && edge.kind == EdgeKind::Implements
                }),
                "invented Go structural edge {} -> {}",
                source.qualified_name,
                target.qualified_name
            );
        }
    }

    #[test]
    fn go_signature_canonicalization_ignores_binding_names_but_preserves_types() {
        assert!(go_signatures_compatible(
            "(buffer []byte): (count int, err error)",
            "(payload []byte): (n int, failure error)"
        ));
        assert!(go_signatures_compatible(
            "(left, right int, values ...string): error",
            "(a, b int, rest ...string): error"
        ));
        assert!(!go_signatures_compatible(
            "(value string): error",
            "(value []byte): error"
        ));
        assert!(!go_signatures_compatible("(): error", "(): bool"));
    }

    #[test]
    fn declarative_schema_relations_resolve_without_graphql_extension_self_edges() {
        let fixtures = [
            (
                "schema/base.graphql",
                "interface Node { id: ID! }\nscalar DateTime\ntype User implements Node { id: ID! }\ntype Post implements Node { id: ID! author: User! }\nunion SearchResult = User | Post\n",
            ),
            (
                "schema/user-extension.graphql",
                "extend type User implements Node { lastSeen: DateTime }\n",
            ),
            (
                "prisma/schema.prisma",
                "enum Access { ADMIN VIEWER }\nmodel Account { id Int @id access Access items Item[] }\nmodel Item { id Int @id owner Account }\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let base_user = CapabilitySymbolQuery::new(&forward, "schema/base.graphql")
            .of_kind("User", SymbolKind::Class);
        let node = CapabilitySymbolQuery::new(&forward, "schema/base.graphql")
            .of_kind("Node", SymbolKind::Interface);
        let date_time = CapabilitySymbolQuery::new(&forward, "schema/base.graphql")
            .of_kind("DateTime", SymbolKind::TypeAlias);
        let extension = CapabilitySymbolQuery::new(&forward, "schema/user-extension.graphql")
            .of_kind("User", SymbolKind::Class);
        let extends = CapabilityReferenceQuery::new(&forward, extension)
            .named("User", ReferenceKind::Extends);
        assert_eq!(
            extends.target_symbol_id.as_ref(),
            Some(&base_user.symbol_id)
        );
        assert_ne!(
            extends.target_symbol_id.as_ref(),
            Some(&extension.symbol_id)
        );
        assert!(forward.edges().iter().any(|edge| {
            edge.source_symbol_id == extension.symbol_id
                && edge.target_symbol_id == base_user.symbol_id
                && edge.kind == EdgeKind::Extends
        }));

        let base_implements = CapabilityReferenceQuery::new(&forward, base_user)
            .named("Node", ReferenceKind::Implements);
        assert_eq!(
            base_implements.target_symbol_id.as_ref(),
            Some(&node.symbol_id)
        );
        let extension_implements = CapabilityReferenceQuery::new(&forward, extension)
            .named("Node", ReferenceKind::Implements);
        assert_eq!(
            extension_implements.target_symbol_id.as_ref(),
            Some(&node.symbol_id)
        );
        let last_seen = CapabilitySymbolQuery::new(&forward, "schema/user-extension.graphql")
            .of_kind("User::lastSeen", SymbolKind::Field);
        let last_seen_type = CapabilityReferenceQuery::new(&forward, last_seen)
            .named("DateTime", ReferenceKind::TypeOf);
        assert_eq!(
            last_seen_type.target_symbol_id.as_ref(),
            Some(&date_time.symbol_id)
        );

        let access = CapabilitySymbolQuery::new(&forward, "prisma/schema.prisma")
            .of_kind("Access", SymbolKind::Enum);
        let account = CapabilitySymbolQuery::new(&forward, "prisma/schema.prisma")
            .of_kind("Account", SymbolKind::Struct);
        let item = CapabilitySymbolQuery::new(&forward, "prisma/schema.prisma")
            .of_kind("Item", SymbolKind::Struct);
        for (owner_name, target_name, target_id) in [
            ("Account::access", "Access", &access.symbol_id),
            ("Account::items", "Item", &item.symbol_id),
            ("Item::owner", "Account", &account.symbol_id),
        ] {
            let owner = CapabilitySymbolQuery::new(&forward, "prisma/schema.prisma")
                .of_kind(owner_name, SymbolKind::Field);
            let relation = CapabilityReferenceQuery::new(&forward, owner)
                .named(target_name, ReferenceKind::TypeOf);
            assert_eq!(relation.target_symbol_id.as_ref(), Some(target_id));
            assert!(forward.documents().iter().any(|document| {
                document.symbol_id() == Some(&owner.symbol_id)
                    && document.kind() == DocumentKind::Symbol
                    && document.qualified_name() == owner_name
            }));
        }
        assert!(forward.documents().iter().any(|document| {
            document.symbol_id() == Some(&last_seen.symbol_id)
                && document.code().contains("lastSeen: DateTime")
        }));
    }

    const SQL_CROSS_LANGUAGE_FIXTURES: [(&str, &str); 2] = [
        (
            "db/schema.sql",
            "CREATE TABLE public.users (id BIGINT PRIMARY KEY);\nCREATE TABLE reporting.orders (id BIGINT PRIMARY KEY, user_id BIGINT REFERENCES public.users(id));\nCREATE VIEW reporting.active_orders AS SELECT o.id FROM reporting.orders o JOIN public.users u ON u.id = o.user_id;\nCREATE FUNCTION reporting.find_users() RETURNS BIGINT AS 'SELECT * FROM public.users' LANGUAGE SQL;\n",
        ),
        (
            "src/repository.ts",
            "export function loadOrders() { db.query('SELECT * FROM public.users JOIN reporting.orders ON reporting.orders.user_id = public.users.id'); db.exec('UPDATE reporting.orders SET id = id'); db.exec('CREATE TABLE audit_missing(id int)'); }\n",
        ),
    ];

    #[test]
    fn sql_schema_and_static_application_queries_form_operation_aware_cross_language_edges() {
        let fixtures = SQL_CROSS_LANGUAGE_FIXTURES;
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let users = CapabilitySymbolQuery::new(&forward, "db/schema.sql")
            .of_kind("public.users", SymbolKind::Table);
        let orders = CapabilitySymbolQuery::new(&forward, "db/schema.sql")
            .of_kind("reporting.orders", SymbolKind::Table);
        let view = CapabilitySymbolQuery::new(&forward, "db/schema.sql")
            .of_kind("reporting.active_orders", SymbolKind::Table);
        let load = capability_symbol(&forward, "src/repository.ts", "loadOrders");
        let function = CapabilitySymbolQuery::new(&forward, "db/schema.sql")
            .of_kind("reporting.find_users", SymbolKind::Function);

        for (owner, target, name, provenance) in [
            (load, users, "public.users", EMBEDDED_SQL_READ_PROVENANCE),
            (
                load,
                orders,
                "reporting.orders",
                EMBEDDED_SQL_READ_PROVENANCE,
            ),
            (
                load,
                orders,
                "reporting.orders",
                EMBEDDED_SQL_WRITE_PROVENANCE,
            ),
            (
                function,
                users,
                "public.users",
                EMBEDDED_SQL_READ_PROVENANCE,
            ),
        ] {
            let reference = forward
                .references()
                .iter()
                .find(|reference| {
                    reference.owner_symbol_id.as_ref() == Some(&owner.symbol_id)
                        && reference.reference_name == name
                        && reference.resolution_provenance == provenance
                })
                .unwrap_or_else(|| {
                    panic!(
                        "missing SQL reference {provenance} {name}: {:?}",
                        forward.references()
                    )
                });
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
            assert_confidence(reference.confidence, EMBEDDED_SQL_CONFIDENCE);
            assert!(forward.edges().iter().any(|edge| {
                edge.source_symbol_id == owner.symbol_id
                    && edge.target_symbol_id == target.symbol_id
                    && edge.kind == EdgeKind::References
                    && edge.provenance == provenance
            }));
        }

        for target in [users, orders] {
            assert!(forward.edges().iter().any(|edge| {
                edge.source_symbol_id == view.symbol_id
                    && edge.target_symbol_id == target.symbol_id
                    && edge.kind == EdgeKind::References
            }));
        }
        let foreign_key = forward
            .references()
            .iter()
            .find(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&orders.symbol_id)
                    && reference.reference_name == "public.users"
            })
            .unwrap_or_else(|| panic!("missing SQL foreign key: {:?}", forward.references()));
        assert_eq!(
            foreign_key.target_symbol_id.as_ref(),
            Some(&users.symbol_id)
        );

        let unresolved = forward
            .references()
            .iter()
            .find(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&load.symbol_id)
                    && reference.reference_name == "audit_missing"
            })
            .unwrap_or_else(|| panic!("missing unresolved embedded DDL reference"));
        assert!(unresolved.target_symbol_id.is_none());
        assert_eq!(
            unresolved.resolution_provenance,
            EMBEDDED_SQL_DDL_UNRESOLVED
        );

        let order_user_id = CapabilitySymbolQuery::new(&forward, "db/schema.sql")
            .of_kind("reporting.orders::user_id", SymbolKind::Field);
        assert!(forward.documents().iter().any(|document| {
            document.symbol_id() == Some(&order_user_id.symbol_id)
                && document.code().contains("BIGINT")
        }));
    }

    #[test]
    fn native_event_channels_bridge_producers_to_javascript_consumers_and_handlers() {
        let fixtures = [
            (
                "ios/DeviceEmitter.m",
                "@implementation DeviceEmitter\n- (void)ready { [self sendEventWithName:@\"device.ready\" body:nil]; }\n@end\n",
            ),
            (
                "android/DeviceModule.kt",
                "class DeviceModule { fun publish() { reactContext.getJSModule(DeviceEventManagerModule.RCTDeviceEventEmitter::class.java).emit(\"device.ready\", null) } }\n",
            ),
            (
                "src/events.ts",
                "import { NativeEventEmitter, NativeModules } from 'react-native';\nexport function handleReady() {}\nconst emitter = new NativeEventEmitter(NativeModules.Device);\nemitter.addListener('device.ready', handleReady);\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let consumer = capability_symbol(
            &forward,
            "src/events.ts",
            "src/events.ts::react-native-event-consumer::device.ready",
        );
        let handler = capability_symbol(&forward, "src/events.ts", "handleReady");
        for producer in [
            capability_symbol(
                &forward,
                "ios/DeviceEmitter.m",
                "ios/DeviceEmitter.m::react-native-event-producer::device.ready",
            ),
            capability_symbol(
                &forward,
                "android/DeviceModule.kt",
                "android/DeviceModule.kt::react-native-event-producer::device.ready",
            ),
        ] {
            assert!(forward.edges().iter().any(|edge| {
                edge.source_symbol_id == producer.symbol_id
                    && edge.target_symbol_id == consumer.symbol_id
                    && edge.provenance == NATIVE_EVENT_BRIDGE_PROVENANCE
            }));
        }
        let handler_reference = forward
            .references()
            .iter()
            .find(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&consumer.symbol_id)
                    && reference.reference_name == "handleReady"
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing event handler reference: {:?}",
                    forward.references()
                )
            });
        assert_eq!(
            handler_reference.target_symbol_id.as_ref(),
            Some(&handler.symbol_id)
        );
    }

    struct NamedReferenceAssertion<'facts> {
        facts: &'facts CanonicalGenerationFacts,
        reference_name: &'facts str,
    }

    impl<'facts> NamedReferenceAssertion<'facts> {
        const fn new(facts: &'facts CanonicalGenerationFacts, reference_name: &'facts str) -> Self {
            Self {
                facts,
                reference_name,
            }
        }

        fn targets(self, target_path: &str, target_qualified_name: &str) {
            let target = capability_symbol(self.facts, target_path, target_qualified_name);
            let reference_name = self.reference_name;
            let facts = self.facts;
            assert!(
                facts.references().iter().any(|reference| {
                    reference.reference_name == reference_name
                        && reference.target_symbol_id.as_ref() == Some(&target.symbol_id)
                }),
                "missing resolved framework reference {reference_name} -> {target_qualified_name}; refs={:?}",
                facts
                    .references()
                    .iter()
                    .filter(|reference| reference.reference_name == reference_name)
                    .collect::<Vec<_>>()
            );
        }
    }

    struct CapabilitySymbolQuery<'facts> {
        facts: &'facts CanonicalGenerationFacts,
        path: &'facts str,
    }

    impl<'facts> CapabilitySymbolQuery<'facts> {
        const fn new(facts: &'facts CanonicalGenerationFacts, path: &'facts str) -> Self {
            Self { facts, path }
        }

        fn of_kind(self, qualified_name: &str, kind: SymbolKind) -> &'facts SymbolInput {
            let file_id = self
                .facts
                .files()
                .iter()
                .find(|file| file.normalized_path == self.path)
                .map_or_else(
                    || panic!("missing capability file {}", self.path),
                    |file| &file.file_id,
                );
            capability_symbol_by(self.facts, file_id, |symbol| {
                symbol.qualified_name == qualified_name && symbol.symbol_kind == kind.as_str()
            })
        }

        fn matching(self, name: &str, qualified_fragment: &str) -> &'facts SymbolInput {
            let file_id = self
                .facts
                .files()
                .iter()
                .find(|file| file.normalized_path == self.path)
                .map_or_else(
                    || panic!("missing capability file {}", self.path),
                    |file| &file.file_id,
                );
            capability_symbol_by(self.facts, file_id, |symbol| {
                symbol.qualified_name.contains(qualified_fragment)
                    && symbol.qualified_name.ends_with(&format!("::{name}"))
            })
        }
    }

    struct CapabilityReferenceQuery<'facts> {
        facts: &'facts CanonicalGenerationFacts,
        owner: &'facts SymbolInput,
    }

    impl<'facts> CapabilityReferenceQuery<'facts> {
        const fn new(facts: &'facts CanonicalGenerationFacts, owner: &'facts SymbolInput) -> Self {
            Self { facts, owner }
        }

        fn named(self, name: &str, kind: ReferenceKind) -> &'facts ReferenceInput {
            let mut references = self.facts.references().iter().filter(|reference| {
                reference.owner_symbol_id.as_ref() == Some(&self.owner.symbol_id)
                    && reference.reference_name == name
                    && reference.reference_kind == kind.as_str()
            });
            let reference = references
                .next()
                .unwrap_or_else(|| panic!("missing capability reference {name}"));
            assert!(
                references.next().is_none(),
                "ambiguous capability reference {name}"
            );
            reference
        }
    }

    fn build_capability_generation(
        fixtures: &[(&str, &str)],
        reverse: bool,
    ) -> CanonicalGenerationFacts {
        build_capability_generation_with_partial_band(fixtures, reverse, false)
    }

    fn build_capability_generation_with_partial_band(
        fixtures: &[(&str, &str)],
        reverse: bool,
        wider_partial_band: bool,
    ) -> CanonicalGenerationFacts {
        build_capability_generation_with_budget(
            fixtures,
            reverse,
            (wider_partial_band, TEST_GENERATION_BYTES),
        )
    }

    fn build_capability_generation_with_budget(
        fixtures: &[(&str, &str)],
        reverse: bool,
        (wider_partial_band, maximum_bytes): (bool, u64),
    ) -> CanonicalGenerationFacts {
        generic_repair::build_generation(
            generic_repair::CapabilityGenerationRequest {
                fixtures,
                reverse,
                wider_partial_band,
                maximum_bytes,
            },
            |_| {},
            || false,
        )
    }

    #[test]
    fn native_clone_analysis_retains_always_on_partial_peer_evidence() {
        let left = r"export function calculate(input: number, limit: number): number {
  log(input);
  const total = input + 1;
  if (total > limit) {
    save(total);
  }





  return total;
}
";
        let right = r"export function calculate(input: number, limit: number): number {
  const total = input + 1;
  if (total > limit) {
    save(total);
  }
  log(input);





  return total;
}
";
        let facts =
            build_capability_generation(&[("src/left.ts", left), ("src/right.ts", right)], false);
        let documents = facts
            .documents()
            .iter()
            .filter(|document| document.qualified_name() == "calculate")
            .collect::<Vec<_>>();
        assert_eq!(documents.len(), 2);
        let document_symbol_ids = documents
            .iter()
            .filter_map(|document| document.symbol_id().map(SymbolId::as_str))
            .collect::<BTreeSet<_>>();
        let mut representatives = BTreeSet::new();
        for document in documents {
            let metadata = serde_json::from_str::<serde_json::Value>(document.metadata_json())
                .unwrap_or_else(|error| panic!("clone metadata was invalid: {error}"));
            assert_eq!(metadata["partial_clone"]["peer_count"], 1);
            assert_eq!(metadata["partial_clone"]["component_size"], 2);
            assert_eq!(metadata["partial_clone"]["maximum_overlap_ppm"], 1_000_000);
            assert_eq!(metadata["partial_clone"]["minimum_overlap_ppm"], 950_000);
            let representative = metadata["partial_clone"]["representative_symbol_id"]
                .as_str()
                .unwrap_or_else(|| panic!("partial clone representative was missing"));
            assert!(document_symbol_ids.contains(representative));
            representatives.insert(representative.to_owned());
            assert_eq!(
                metadata["partial_clone"]["listed_peer_symbol_ids"]
                    .as_array()
                    .map(Vec::len),
                Some(1)
            );
        }
        assert_eq!(representatives.len(), 1);
    }

    #[test]
    fn partial_clone_components_remain_single_beyond_the_display_peer_cap() {
        let mut candidates = (0..MAXIMUM_LISTED_CLONE_PEERS.saturating_add(2))
            .map(|index| CloneAnalysisCandidate {
                file_index: index,
                symbol_index: 0,
                language: SourceLanguage::TypeScript,
                profile: None,
                prefilter: CloneProfilePrefilter::default(),
                syntactic_claimed: false,
                first_partial_band_hit: false,
                partial_peer_count: 0,
                maximum_overlap_ppm: 0,
                minimum_overlap_ppm: 0,
                listed_peer_indexes: [None; MAXIMUM_LISTED_CLONE_PEERS],
                listed_peer_count: 0,
                partial_component_parent: index,
                partial_component_rank: 0,
            })
            .collect::<Vec<_>>();
        for left in 0..candidates.len() {
            for right in left.saturating_add(1)..candidates.len() {
                link_partial_candidates(PartialCloneLink {
                    candidates: &mut candidates,
                    left,
                    right,
                    overlap_ppm: DUPLICATE_PARTIAL_DEFAULT_OVERLAP_PPM,
                    minimum_overlap_ppm: DUPLICATE_PARTIAL_DEFAULT_OVERLAP_PPM,
                })
                .unwrap_or_else(|_| panic!("partial clone component link failed"));
            }
        }
        let root = partial_clone_component_root(&candidates, 0)
            .unwrap_or_else(|_| panic!("partial clone component root failed"));
        for (index, candidate) in candidates.iter().enumerate() {
            assert_eq!(
                candidate.partial_peer_count,
                u32::try_from(candidates.len().saturating_sub(1)).unwrap_or(u32::MAX)
            );
            assert_eq!(candidate.listed_peer_count, MAXIMUM_LISTED_CLONE_PEERS);
            assert_eq!(
                partial_clone_component_root(&candidates, index)
                    .unwrap_or_else(|_| panic!("partial clone component root failed")),
                root
            );
        }
    }

    #[test]
    fn clone_analysis_abstains_for_cross_domain_shape_lookalikes() {
        let sessions = r"export async function revokeSession(id: string) {
  const result = await sessions.revoke(id);
  const mapped = mapSessionResult(result);
  if (mapped.ok) {
    recordSessionAudit(mapped);
  }




  return mapped;
}
";
        let billing = r"export async function cancelInvoice(number: InvoiceId) {
  const outcome = await invoices.cancel(number);
  const response = mapInvoiceResult(outcome);
  if (response.accepted) {
    recordBillingLedger(response);
  }




  return response;
}
";
        let facts = build_capability_generation(
            &[
                ("src/sessions/revoke.ts", sessions),
                ("src/billing/cancel.ts", billing),
            ],
            false,
        );
        for document in facts.documents().iter().filter(|document| {
            matches!(document.qualified_name(), "revokeSession" | "cancelInvoice")
        }) {
            let metadata = serde_json::from_str::<serde_json::Value>(document.metadata_json())
                .unwrap_or_else(|error| panic!("clone metadata was invalid: {error}"));
            assert_eq!(metadata["near_clone_compatible"], false);
            assert!(metadata["partial_clone"].is_null());
        }
    }

    #[test]
    fn clone_analysis_keeps_same_file_shape_with_shared_semantic_callee() {
        let source = r"export function firstClone(input: number) {
  const offset = input + 2;
  const doubled = offset * 3;
  const bounded = Math.max(doubled, 1);
  const normalized = bounded / 2;
  if (normalized > 9) {
    const adjusted = normalized - 4;
    return adjusted * 2;
  }
  const fallback = normalized + 5;
  return fallback * 3;
}
export function secondClone(value: number) {
  const delta = value + 6;
  const scaled = delta * 7;
  const clamped = Math.max(scaled, 5);
  const ratio = clamped / 4;
  if (ratio > 21) {
    const reduced = ratio - 8;
    return reduced * 6;
  }
  const alternative = ratio + 11;
  return alternative * 7;
}
";
        let facts = build_capability_generation(&[("src/clones.ts", source)], false);
        let documents = facts
            .documents()
            .iter()
            .filter(|document| matches!(document.qualified_name(), "firstClone" | "secondClone"))
            .collect::<Vec<_>>();
        assert_eq!(documents.len(), 2);
        for document in documents {
            let metadata = serde_json::from_str::<serde_json::Value>(document.metadata_json())
                .unwrap_or_else(|error| panic!("clone metadata was invalid: {error}"));
            assert_eq!(metadata["near_clone_compatible"], true);
            assert!(metadata["partial_clone"].is_null());
        }
    }

    #[test]
    fn wider_partial_clone_band_adds_only_the_opt_in_080_to_095_evidence() {
        let mut common_statements = String::new();
        for index in 0..40 {
            append_fixture_text(
                &mut common_statements,
                format_args!("  shared_{index}(input);\n"),
            );
        }
        let mut divergent_statements = String::new();
        for index in 0..40 {
            if index < 4 {
                append_fixture_text(
                    &mut divergent_statements,
                    format_args!("  if (input > {index}) {{ alternate_{index}(input); }}\n"),
                );
            } else {
                append_fixture_text(
                    &mut divergent_statements,
                    format_args!("  shared_{index}(input);\n"),
                );
            }
        }
        let left = format!(
            "export function broadAlpha(input: number): number {{\n{common_statements}  return input;\n}}\n"
        );
        let right = format!(
            "export function broadBeta(input: number): number {{\n{divergent_statements}  return input;\n}}\n"
        );
        let fixtures = [
            ("src/broad-left.ts", left.as_str()),
            ("src/broad-right.ts", right.as_str()),
        ];
        let default = build_capability_generation_with_partial_band(&fixtures, false, false);
        assert!(default.documents().iter().all(|document| {
            if !matches!(document.qualified_name(), "broadAlpha" | "broadBeta") {
                return true;
            }
            serde_json::from_str::<serde_json::Value>(document.metadata_json())
                .is_ok_and(|metadata| metadata["partial_clone"].is_null())
        }));

        let wider = build_capability_generation_with_partial_band(&fixtures, false, true);
        let documents = wider
            .documents()
            .iter()
            .filter(|document| matches!(document.qualified_name(), "broadAlpha" | "broadBeta"))
            .collect::<Vec<_>>();
        assert_eq!(documents.len(), 2);
        for document in documents {
            let metadata = serde_json::from_str::<serde_json::Value>(document.metadata_json())
                .unwrap_or_else(|error| panic!("wide-band clone metadata was invalid: {error}"));
            let overlap = metadata["partial_clone"]["maximum_overlap_ppm"]
                .as_u64()
                .unwrap_or_default();
            assert!((800_000..950_000).contains(&overlap), "{metadata}");
            assert_eq!(metadata["partial_clone"]["minimum_overlap_ppm"], 800_000);
            assert_eq!(metadata["partial_clone"]["peer_count"], 1);
        }
    }

    fn capability_file_symbol<'facts>(
        facts: &'facts CanonicalGenerationFacts,
        path: &str,
    ) -> &'facts SymbolInput {
        let file_id = facts
            .files()
            .iter()
            .find(|file| file.normalized_path == path)
            .map_or_else(
                || panic!("missing capability file {path}"),
                |file| &file.file_id,
            );
        capability_symbol_by(facts, file_id, |symbol| {
            symbol.symbol_kind == SymbolKind::File.as_str()
        })
    }

    fn capability_symbol<'facts>(
        facts: &'facts CanonicalGenerationFacts,
        path: &str,
        qualified_name: &str,
    ) -> &'facts SymbolInput {
        let file_id = facts
            .files()
            .iter()
            .find(|file| file.normalized_path == path)
            .map_or_else(
                || panic!("missing capability file {path}"),
                |file| &file.file_id,
            );
        capability_symbol_by(facts, file_id, |symbol| {
            symbol.qualified_name == qualified_name
        })
    }

    fn capability_reference_in_file<'facts>(
        facts: &'facts CanonicalGenerationFacts,
        path: &str,
        name_suffix: &str,
    ) -> &'facts ReferenceInput {
        let file_id = facts
            .files()
            .iter()
            .find(|file| file.normalized_path == path)
            .map_or_else(
                || panic!("missing capability file {path}"),
                |file| &file.file_id,
            );
        facts
            .references()
            .iter()
            .find(|reference| {
                &reference.file_id == file_id && reference.reference_name.ends_with(name_suffix)
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing capability reference {name_suffix} in {path}: {:?}",
                    facts.references()
                )
            })
    }

    fn capability_symbol_by<'facts, Match>(
        facts: &'facts CanonicalGenerationFacts,
        file_id: &FileId,
        matches: Match,
    ) -> &'facts SymbolInput
    where
        Match: Fn(&SymbolInput) -> bool,
    {
        let mut symbols = facts
            .symbols()
            .iter()
            .filter(|symbol| &symbol.file_id == file_id && matches(symbol));
        let symbol = symbols.next().unwrap_or_else(|| {
            panic!(
                "missing capability symbol in {file_id}; available={:?}",
                facts
                    .symbols()
                    .iter()
                    .filter(|symbol| &symbol.file_id == file_id)
                    .map(|symbol| (&symbol.symbol_kind, &symbol.qualified_name))
                    .collect::<Vec<_>>()
            )
        });
        assert!(symbols.next().is_none(), "ambiguous capability symbol");
        symbol
    }

    fn assert_generation_projection(
        facts: &CanonicalGenerationFacts,
        expected: (usize, usize, usize, usize, usize),
    ) {
        assert_eq!(
            (
                facts.files().len(),
                facts.symbols().len(),
                facts.edges().len(),
                facts.references().len(),
                facts.documents().len(),
            ),
            expected
        );
    }

    fn write_parser_only_project(root: &std::path::Path) {
        for (path, source, _) in PARSER_ONLY_FIXTURES {
            let target = root.join(path);
            let parent = target
                .parent()
                .unwrap_or_else(|| panic!("parser-only fixture had no parent: {path}"));
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(&target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
    }

    fn write_admitted_family_project(root: &std::path::Path) {
        for (path, source, _) in ADMITTED_FAMILY_FIXTURES {
            let target = root.join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("admitted-family fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
    }

    fn write_generic_family_project(root: &std::path::Path) {
        for (path, source, _) in GENERIC_FAMILY_FIXTURES {
            let target = root.join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("generic-family fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
    }

    fn write_custom_family_project(root: &std::path::Path) {
        for (path, source, _) in CUSTOM_FAMILY_FIXTURES {
            let target = root.join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("custom-family fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
    }

    fn assert_parser_only_generation(facts: &CanonicalGenerationFacts) {
        assert_eq!(facts.files().len(), PARSER_ONLY_FILE_COUNT);
        assert_eq!(facts.symbols().len(), PARSER_ONLY_FILE_COUNT);
        assert!(
            facts
                .symbols()
                .iter()
                .all(|symbol| symbol.symbol_kind == SymbolKind::File.as_str())
        );
        assert_eq!(facts.references(), []);
        assert_eq!(facts.edges(), []);
        assert_eq!(
            facts
                .documents()
                .iter()
                .filter(|document| document.symbol_id().is_some())
                .count(),
            PARSER_ONLY_FILE_COUNT
        );
        assert!(
            facts
                .documents()
                .iter()
                .all(|document| document.symbol_id().is_some())
        );
        for (path, _, language) in PARSER_ONLY_FIXTURES {
            assert!(facts.files().iter().any(|file| {
                file.normalized_path == path && file.language == language.as_str()
            }));
        }
    }

    #[test]
    fn manifest_content_classifier_skips_only_locked_unsupported_languages() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create manifest fixture: {error}"));
        write_classifier_negatives(directory.path());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open manifest fixture: {error}"));

        for (path, _) in CONTENT_CLASSIFIER_NEGATIVES {
            let result = read_manifest_entry(
                read_manifest_input(&source_root, directory.path(), path),
                || false,
            );
            assert_matches!(result, Ok(None), "{path}: {result:?}");
        }
    }

    #[test]
    fn spilled_parse_batch_reuses_one_extractor_for_each_language() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create parser reuse fixture: {error}"));
        for (path, source) in [
            ("src/first.ts", "export function first() { return 1; }\n"),
            ("src/second.ts", "export function second() { return 2; }\n"),
        ] {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("parser reuse fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create parser reuse parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write parser reuse fixture: {error}"));
        }
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open parser reuse fixture: {error}"));
        let mut pool = SpilledExtractorPool::default();
        for path in ["src/first.ts", "src/second.ts"] {
            let manifest = read_manifest_entry(
                read_manifest_input(&source_root, directory.path(), path),
                || false,
            )
            .unwrap_or_else(|_| panic!("could not read parser reuse manifest: {path}"))
            .unwrap_or_else(|| panic!("parser reuse manifest was skipped: {path}"));
            let extracted = pool
                .extract(&source_root, &manifest, || false)
                .unwrap_or_else(|_| panic!("could not extract parser reuse fixture: {path}"));
            assert_eq!(extracted.language, SourceLanguage::TypeScript);
        }
        assert_eq!(pool.extractors.len(), 1);
        assert_eq!(pool.initializations, 1);
    }

    #[test]
    fn manifest_skip_does_not_mask_invalid_utf8_or_read_failure() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create manifest failure fixture: {error}"));
        let invalid_path = "unrelated/invalid.cls";
        fs::create_dir_all(directory.path().join("unrelated"))
            .unwrap_or_else(|error| panic!("could not create invalid fixture parent: {error}"));
        fs::write(directory.path().join(invalid_path), [0xff_u8])
            .unwrap_or_else(|error| panic!("could not write invalid UTF-8 fixture: {error}"));
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open manifest failure fixture: {error}"));
        assert!(
            read_manifest_entry(
                read_manifest_input(&source_root, directory.path(), invalid_path),
                || false,
            )
            .is_err()
        );

        let removed_path = "unrelated/removed.cls";
        fs::write(
            directory.path().join(removed_path),
            "public class Removed {}\n",
        )
        .unwrap_or_else(|error| panic!("could not write removed fixture: {error}"));
        let input = read_manifest_input(&source_root, directory.path(), removed_path);
        fs::remove_file(directory.path().join(removed_path))
            .unwrap_or_else(|error| panic!("could not remove read fixture: {error}"));
        assert!(read_manifest_entry(input, || false).is_err());
    }

    fn read_manifest_input(
        source_root: &SourceRoot,
        root: &std::path::Path,
        path: &str,
    ) -> ReadManifestInput {
        let normalized = NormalizedPath::parse(path)
            .unwrap_or_else(|error| panic!("invalid manifest fixture path {path}: {error}"));
        let byte_size = fs::metadata(root.join(path))
            .unwrap_or_else(|error| panic!("could not stat {path}: {error}"))
            .len();
        ReadManifestInput {
            source_root: source_root.clone(),
            discovered: DiscoveredSource::new(normalized, byte_size),
            global_limits: config(SERIAL_WORKERS).limits.source_limits,
        }
    }

    fn write_classifier_negatives(root: &std::path::Path) {
        for (path, source) in CONTENT_CLASSIFIER_NEGATIVES {
            let target = root.join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("classifier fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn content_classifier_skips_only_locked_unrelated_candidates() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create classifier fixture: {error}"));
        write_classifier_negatives(directory.path());
        fs::write(directory.path().join("admitted.json"), "{}\n")
            .unwrap_or_else(|error| panic!("could not write admitted fixture: {error}"));

        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        assert_eq!(serial.report().discovered_files(), 1);
        assert_eq!(serial.facts().files().len(), 1);
        assert_eq!(serial.facts().files()[0].normalized_path, "admitted.json");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn module_aliases_lexical_shadowing_and_overloads_resolve_deterministically() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create module resolver fixture: {error}"));
        write_module_project(directory.path());
        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        let facts = ModuleResolverFacts {
            facts: generation.facts(),
        };
        let use_id = facts.symbol("src/consumer.ts", "use");
        let shadow_id = facts.symbol("src/consumer.ts", "shadow");
        let default_id = facts.symbol("src/api.ts", "DefaultClient");
        let service_id = facts.symbol("src/api.ts", "Service");
        let make_id = facts.symbol("src/api.ts", "make");
        let shadow_service_id = facts.symbol("src/consumer.ts", "shadow::RemoteService");
        let parse_implementation = facts.parse_implementation();
        let import_service = facts.import_declaration_reference("src/consumer.ts", "Service");

        assert_eq!(import_service.target_symbol_id.as_ref(), Some(&service_id));
        assert_eq!(
            import_service.resolution_provenance,
            IMPORT_BINDING_PROVENANCE
        );

        for (name, target) in [
            ("DefaultClient", default_id),
            ("RemoteService", service_id),
            ("parse", parse_implementation),
            ("api.make", make_id),
        ] {
            let reference = facts.reference(&use_id, name);
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target));
            assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        }
        let package_reference = facts.reference(&use_id, "external");
        assert!(package_reference.target_symbol_id.is_none());
        assert_eq!(
            package_reference.resolution_provenance,
            EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
        );
        let shadow_reference = facts.reference(&shadow_id, "RemoteService");
        assert_eq!(
            shadow_reference.target_symbol_id.as_ref(),
            Some(&shadow_service_id)
        );
        assert_eq!(
            shadow_reference.resolution_provenance,
            EXACT_LEXICAL_PROVENANCE
        );
        facts.assert_use_document_terms(&use_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slang_and_wesl_module_imports_resolve_to_exact_shader_files() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create shader resolver fixture: {error}"));
        for (path, source) in [
            (
                "renderer/common-math.slang",
                "module renderer.common_math;\nfloat helper(float value) { return value; }\n",
            ),
            (
                "renderer/material.slang",
                "module renderer.material;\nimport renderer.common_math;\nvoid materialMain() {}\n",
            ),
            (
                "lighting/common.wesl",
                "fn light_value() -> f32 { return 1.0; }\n",
            ),
            (
                "lighting/material.wesl",
                "import package::lighting::common;\n@fragment\nfn material_main() -> @location(0) vec4<f32> { return vec4<f32>(); }\n",
            ),
        ] {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("shader fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create shader fixture parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write shader fixture: {error}"));
        }
        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        for (path, reference_name) in [
            ("renderer/material.slang", "renderer/common-math"),
            ("lighting/material.wesl", "package::lighting::common"),
        ] {
            let file_id = generation
                .facts()
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .map_or_else(
                    || panic!("missing shader file {path}"),
                    |file| &file.file_id,
                );
            let reference = generation
                .facts()
                .references()
                .iter()
                .find(|reference| {
                    &reference.file_id == file_id
                        && reference.reference_kind == ReferenceKind::Imports.as_str()
                        && reference.reference_name == reference_name
                })
                .unwrap_or_else(|| panic!("missing shader import {path}:{reference_name}"));
            assert!(
                reference.target_symbol_id.is_some(),
                "{path}:{reference_name}"
            );
            assert_eq!(reference.resolution_provenance, MODULE_IMPORT_PROVENANCE);
        }
    }

    const TERRAFORM_RESOLUTION_FIXTURES: [(&str, &str); 8] = [
        (
            "infra/main.tf",
            "locals {\n  name = \"${var.prefix}-app\"\n}\nresource \"aws_instance\" \"web\" {\n  ami = data.aws_ami.ubuntu.id\n  tags = { Name = local.name }\n}\nresource \"aws_instance\" \"db\" {\n  ami  = aws_instance.web.ami\n  zone = var.missing\n  peer = aws_instance.db.id\n  logs = aws_s3_bucket.logs.arn\n  dup  = var.duplicated\n}\nmodule \"vpc\" {\n  source = \"../modules/vpc\"\n}\noutput \"vpc_id\" {\n  value = module.vpc.vpc_id\n}\n",
        ),
        (
            "infra/variables.tf",
            "variable \"prefix\" {\n  type = string\n}\nvariable \"duplicated\" {}\n",
        ),
        ("infra/more.tf", "variable \"duplicated\" {}\n"),
        (
            "infra/data.tf",
            "data \"aws_ami\" \"ubuntu\" {}\ndata \"aws_s3_bucket\" \"logs\" {}\n",
        ),
        (
            "infra/terraform.properties",
            "var.missing=present-in-properties\n",
        ),
        (
            "modules/vpc/variables.tf",
            "variable \"prefix\" {\n  type = string\n}\n",
        ),
        (
            "modules/vpc/main.tf",
            "resource \"aws_vpc\" \"this\" {\n  tags = { Name = var.prefix }\n}\n",
        ),
        (
            "modules/vpc/outputs.tf",
            "output \"vpc_id\" { value = aws_vpc.this.id }\n",
        ),
    ];

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn terraform_addresses_resolve_within_their_module_directory_only() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create Terraform resolver fixture: {error}"));
        for (path, source) in TERRAFORM_RESOLUTION_FIXTURES {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("Terraform fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }

        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        let facts = ModuleResolverFacts {
            facts: serial.facts(),
        };
        let local_name = facts.symbol("infra/main.tf", "local.name");
        let web = facts.symbol("infra/main.tf", "aws_instance.web");
        let db = facts.symbol("infra/main.tf", "aws_instance.db");
        let output = facts.symbol("infra/main.tf", "output.vpc_id");
        for (owner, name, target, provenance) in [
            (
                &local_name,
                "var.prefix",
                facts.symbol("infra/variables.tf", "var.prefix"),
                EXACT_PROJECT_PROVENANCE,
            ),
            (
                &web,
                "data.aws_ami.ubuntu",
                facts.symbol("infra/data.tf", "data.aws_ami.ubuntu"),
                EXACT_PROJECT_PROVENANCE,
            ),
            (
                &web,
                "local.name",
                local_name.clone(),
                EXACT_SAME_FILE_PROVENANCE,
            ),
            (
                &db,
                "aws_instance.web",
                web.clone(),
                EXACT_SAME_FILE_PROVENANCE,
            ),
            (
                &output,
                "module.vpc",
                facts.symbol("infra/main.tf", "module.vpc"),
                EXACT_SAME_FILE_PROVENANCE,
            ),
            (
                &facts.symbol("modules/vpc/main.tf", "aws_vpc.this"),
                "var.prefix",
                facts.symbol("modules/vpc/variables.tf", "var.prefix"),
                EXACT_PROJECT_PROVENANCE,
            ),
            (
                &facts.symbol("modules/vpc/outputs.tf", "output.vpc_id"),
                "aws_vpc.this",
                facts.symbol("modules/vpc/main.tf", "aws_vpc.this"),
                EXACT_PROJECT_PROVENANCE,
            ),
        ] {
            let reference = facts.reference(owner, name);
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target), "{name}");
            assert_eq!(reference.resolution_provenance, provenance, "{name}");
        }
        // A missing address is not satisfied by a same-named properties key,
        // a data source's display name never stands in for a resource
        // address, duplicated declarations stay ambiguous, and a block never
        // resolves to itself.
        for name in [
            "var.missing",
            "aws_s3_bucket.logs",
            "var.duplicated",
            "aws_instance.db",
        ] {
            let reference = facts.reference(&db, name);
            assert_eq!(reference.target_symbol_id, None, "{name}");
            assert_eq!(
                reference.resolution_provenance, UNRESOLVED_PROVENANCE,
                "{name}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bg3_template_uuid_references_resolve_across_files_unless_ambiguous() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create BG3 resolver fixture: {error}"));
        let template = |name: &str, uuid: &str| {
            format!(
                "<save><region id=\"Templates\"><node id=\"GameObjects\"><children><node id=\"GameObject\"><attribute id=\"UUID\" value=\"{uuid}\"/><attribute id=\"Name\" value=\"{name}\"/></node></children></node></region></save>\n"
            )
        };
        for (path, source) in [
            (
                "Public/Mod/RootTemplates/Sword.lsx",
                template("WPN_Sword", "11111111-1111-1111-1111-111111111111"),
            ),
            (
                "Public/Mod/RootTemplates/ShieldA.lsx",
                template("ARM_Shield_A", "55555555-5555-5555-5555-555555555555"),
            ),
            (
                "Public/Mod/RootTemplates/ShieldB.lsx",
                template("ARM_Shield_B", "55555555-5555-5555-5555-555555555555"),
            ),
            (
                "Public/Mod/RootTemplates/Items.lsx",
                "<save><region id=\"Templates\"><node id=\"GameObject\"><attribute id=\"Name\" value=\"MAG_Sword\"/><attribute id=\"ParentTemplateId\" value=\"11111111-1111-1111-1111-111111111111\"/><attribute id=\"ShieldTemplateId\" value=\"55555555-5555-5555-5555-555555555555\"/></node></region></save>\n".to_owned(),
            ),
        ] {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("BG3 fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }

        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        let facts = ModuleResolverFacts {
            facts: serial.facts(),
        };
        let item = facts.symbol(
            "Public/Mod/RootTemplates/Items.lsx",
            "Public/Mod/RootTemplates/Items.lsx::Templates::MAG_Sword",
        );
        let parent = facts.reference(&item, "11111111-1111-1111-1111-111111111111");
        assert_eq!(
            parent.target_symbol_id.as_ref(),
            Some(&facts.symbol(
                "Public/Mod/RootTemplates/Sword.lsx",
                "11111111-1111-1111-1111-111111111111"
            ))
        );
        assert_eq!(parent.resolution_provenance, EXACT_PROJECT_PROVENANCE);
        let duplicated = facts.reference(&item, "55555555-5555-5555-5555-555555555555");
        assert_eq!(
            duplicated.target_symbol_id, None,
            "duplicate UUIDs stay ambiguous"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ada_and_vhdl_units_resolve_by_declared_name_not_filename_guessing() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create Ada/VHDL resolver fixture: {error}"));
        for (path, source) in [
            (
                "ada/project-math.ads",
                "package Project.Math is\n   function Normalize (Value : Integer) return Integer;\nend Project.Math;\n",
            ),
            (
                "ada/project-math.adb",
                "package body Project.Math is\n   function Normalize (Value : Integer) return Integer is\n   begin\n      return Value;\n   end Normalize;\nend Project.Math;\n",
            ),
            (
                "ada/consumer.adb",
                "with Project.Math;\nuse Project.Math;\npackage body Consumer is\n   function Run (Value : Integer) return Integer is\n   begin\n      return Normalize (Value);\n   end Run;\nend Consumer;\n",
            ),
            (
                "rtl/unrelated_filename.vhd",
                "package math_pkg is\n   function Normalize (Value : integer) return integer;\nend package;\npackage body math_pkg is\n   function Normalize (Value : integer) return integer is\n   begin\n      return Value;\n   end function;\nend package body;\n",
            ),
            (
                "rtl/consumer.vhd",
                "use work.math_pkg.all;\nentity Consumer is\nend entity;\narchitecture rtl of Consumer is\n   function Compute (Value : integer) return integer is\n   begin\n      return Normalize(Value);\n   end function;\nbegin\nend architecture;\n",
            ),
        ] {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("Ada/VHDL fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }

        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        let facts = ModuleResolverFacts {
            facts: serial.facts(),
        };

        let ada_run = facts.symbol("ada/consumer.adb", "consumer::run");
        let ada_declaration = facts.symbol("ada/project-math.ads", "project.math::normalize");
        let ada_call = facts.reference(&ada_run, "normalize");
        let ada_target = ada_call
            .target_symbol_id
            .as_ref()
            .and_then(|target| {
                serial
                    .facts()
                    .symbols()
                    .iter()
                    .find(|symbol| &symbol.symbol_id == target)
            })
            .map(|symbol| {
                let path = serial
                    .facts()
                    .files()
                    .iter()
                    .find(|file| file.file_id == symbol.file_id)
                    .map_or("<missing>", |file| file.normalized_path.as_str());
                (
                    path,
                    symbol.qualified_name.as_str(),
                    symbol.declaration_only,
                )
            });
        assert_eq!(
            ada_call.target_symbol_id.as_ref(),
            Some(&ada_declaration),
            "unexpected Ada target: {ada_target:?}"
        );
        assert_eq!(ada_call.resolution_provenance, IMPORT_BINDING_PROVENANCE);

        let vhdl_compute = facts.symbol("rtl/consumer.vhd", "rtl::compute");
        let vhdl_call = facts.reference(&vhdl_compute, "normalize");
        let vhdl_target = vhdl_call
            .target_symbol_id
            .as_ref()
            .unwrap_or_else(|| panic!("VHDL package call did not resolve"));
        assert!(serial.facts().symbols().iter().any(|symbol| {
            &symbol.symbol_id == vhdl_target
                && symbol.qualified_name == "math_pkg::normalize"
                && symbol.symbol_kind == SymbolKind::Function.as_str()
                && !symbol.declaration_only
        }));
        assert_eq!(vhdl_call.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ambiguous_vhdl_unit_names_fail_closed() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create VHDL ambiguity fixture: {error}"));
        for (path, source) in [
            (
                "rtl/first.vhd",
                "package duplicate_pkg is\n   function Choose return integer;\nend package;\n",
            ),
            (
                "other/second.vhd",
                "package duplicate_pkg is\n   function Choose return integer;\nend package;\n",
            ),
            (
                "rtl/consumer.vhd",
                "use work.duplicate_pkg.all;\nentity Consumer is end entity;\narchitecture rtl of Consumer is\n   function Run return integer is\n   begin\n      return Choose;\n   end function;\nbegin\nend architecture;\n",
            ),
        ] {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("VHDL ambiguity fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }
        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        let import = generation
            .facts()
            .references()
            .iter()
            .find(|reference| {
                reference.reference_kind == ReferenceKind::Imports.as_str()
                    && reference.reference_name == "duplicate_pkg"
            })
            .unwrap_or_else(|| panic!("missing ambiguous VHDL import"));
        assert!(import.target_symbol_id.is_none());
        assert_eq!(import.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }

    const BORROWED_LANGUAGE_FIXTURES: [(&str, &str); 7] = [
        ("clj/demo/util.clj", "(ns demo.util)\n(defn helper [x] x)\n"),
        (
            "clj/demo/core.clj",
            "(ns demo.core\n  (:require [demo.util :refer [helper]]))\n(defn- local [x] x)\n(defn greet [n] (local (helper n)))\n",
        ),
        (
            "lisp/util.lisp",
            "(defpackage :demo.util (:use :cl))\n(in-package :demo.util)\n(defun twice (x) x)\n",
        ),
        (
            "lisp/core.lisp",
            "(defpackage :demo.core (:use :cl) (:import-from :demo.util #:twice))\n(in-package :demo.core)\n(defun run (n) (twice n))\n",
        ),
        ("res/Utils.res", "let shout = (x) => x\n"),
        (
            "res/App.res",
            "open Utils\nlet local = (x) => x\nlet run = () => local(shout(1))\n",
        ),
        (
            "sol/Vault.sol",
            "contract Vault {\n  function helper(uint a) private returns (uint) { return a; }\n  function deposit(uint a) public returns (uint) { return helper(a); }\n  function apply(function(uint) internal pure returns (uint) helper, uint a) internal pure returns (uint) { return helper(a); }\n}\n",
        ),
    ];

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn borrowed_language_calls_resolve_through_lexical_and_project_rules() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create borrowed-language fixture: {error}"));
        for (path, source) in BORROWED_LANGUAGE_FIXTURES {
            let target = directory.path().join(path);
            fs::create_dir_all(
                target
                    .parent()
                    .unwrap_or_else(|| panic!("borrowed-language fixture had no parent: {path}")),
            )
            .unwrap_or_else(|error| panic!("could not create {path} parent: {error}"));
            fs::write(target, source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }

        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        let facts = ModuleResolverFacts {
            facts: serial.facts(),
        };
        for (caller_path, caller, name, target_path, target, provenance) in [
            (
                "clj/demo/core.clj",
                "greet",
                "local",
                "clj/demo/core.clj",
                "local",
                EXACT_SAME_FILE_PROVENANCE,
            ),
            // Explicitly referred and imported names bind nothing, so they
            // resolve through the project-wide rule like v1's by-name lookup.
            (
                "clj/demo/core.clj",
                "greet",
                "helper",
                "clj/demo/util.clj",
                "helper",
                EXACT_PROJECT_PROVENANCE,
            ),
            (
                "lisp/core.lisp",
                "run",
                "twice",
                "lisp/util.lisp",
                "twice",
                EXACT_PROJECT_PROVENANCE,
            ),
            (
                "res/App.res",
                "run",
                "local",
                "res/App.res",
                "local",
                EXACT_SAME_FILE_PROVENANCE,
            ),
            (
                "res/App.res",
                "run",
                "shout",
                "res/Utils.res",
                "shout",
                EXACT_PROJECT_PROVENANCE,
            ),
            (
                "sol/Vault.sol",
                "Vault::deposit",
                "helper",
                "sol/Vault.sol",
                "Vault::helper",
                EXACT_SAME_FILE_PROVENANCE,
            ),
        ] {
            let call = facts.reference(&facts.symbol(caller_path, caller), name);
            assert_eq!(
                call.target_symbol_id.as_ref(),
                Some(&facts.symbol(target_path, target)),
                "{caller_path}:{caller} -> {name}"
            );
            assert_eq!(call.resolution_provenance, provenance, "{caller}->{name}");
        }
        // A parameter that shadows a contract member stays unresolved instead
        // of reaching the same-named member.
        let shadowed = facts.reference(&facts.symbol("sol/Vault.sol", "Vault::apply"), "helper");
        assert_eq!(shadowed.target_symbol_id, None, "Vault::apply -> helper");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn typescript_paths_without_base_url_resolve_from_the_config_directory() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create TypeScript alias fixture: {error}"));
        fs::create_dir(directory.path().join("src"))
            .unwrap_or_else(|error| panic!("could not create alias source directory: {error}"));
        for (path, source) in [
            (
                "tsconfig.json",
                r##"{
  "compilerOptions": {
    "moduleResolution": "bundler",
    "paths": {
      "~/*": ["./src/*"],
      "#domain/*": ["./src/*"]
    }
  },
  "include": ["src/**/*.ts", "src/**/*.tsx"]
}
"##,
            ),
            (
                "src/widget.tsx",
                "export function Widget() { return <div />; }\n",
            ),
            (
                "src/consumer.tsx",
                "import { Widget } from '#domain/widget';\nexport function Consumer() { return <Widget />; }\n",
            ),
            (
                "src/invalid.tsx",
                "import { Missing } from '#domain/missing';\nexport function Invalid() { return <Missing />; }\n",
            ),
        ] {
            fs::write(directory.path().join(path), source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }

        let serial = build(directory.path(), SERIAL_WORKERS).await;
        let parallel = build(directory.path(), PARALLEL_WORKERS).await;
        assert_eq!(serial.facts().digest(), parallel.facts().digest());
        let facts = ModuleResolverFacts {
            facts: serial.facts(),
        };
        let widget = facts.symbol("src/widget.tsx", "Widget");
        let consumer = facts.symbol("src/consumer.tsx", "Consumer");
        let imported = facts.import_declaration_reference("src/consumer.tsx", "Widget");
        assert_eq!(imported.target_symbol_id.as_ref(), Some(&widget));
        assert_eq!(imported.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        let rendered = facts.reference(&consumer, "Widget");
        assert_eq!(rendered.target_symbol_id.as_ref(), Some(&widget));
        assert_eq!(rendered.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        let invalid = facts.import_declaration_reference("src/invalid.tsx", "Missing");
        assert!(invalid.target_symbol_id.is_none());
        assert_eq!(invalid.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }

    #[test]
    fn typescript_config_comment_stripping_preserves_quoted_comment_tokens() {
        let stripped = strip_typescript_config_comments(
            r#"{
  // one line comment
  "url": "https://example.invalid/a//b",
  "escaped": "quote: \"/*literal*/\"",
  /* one block comment */
  "value": 1
}"#,
        );
        let parsed: Value = serde_json::from_str(&stripped)
            .unwrap_or_else(|error| panic!("stripped TypeScript config was invalid: {error}"));
        assert_eq!(parsed["url"], "https://example.invalid/a//b");
        assert_eq!(parsed["escaped"], "quote: \"/*literal*/\"");
        assert_eq!(parsed["value"], 1);
        assert!(!stripped.contains("one line comment"));
        assert!(!stripped.contains("one block comment"));
        assert_eq!(
            strip_typescript_config_comments("{\"value\":1}/*unterminated"),
            "{\"value\":1}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rust_bare_child_imports_resolve_without_weakening_external_boundaries() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create Rust module fixture: {error}"));
        fs::create_dir(directory.path().join("local"))
            .unwrap_or_else(|error| panic!("could not create local module directory: {error}"));
        for (path, source) in [
            (
                "lib.rs",
                "mod external_consumer;\nmod external_shadow;\nmod local;\n",
            ),
            (
                "local.rs",
                "mod helper;\nuse helper::{Helper, LIMIT};\npub fn consume() -> usize { Helper::new() + LIMIT }\n",
            ),
            (
                "local/helper.rs",
                "pub(super) struct Helper;\nimpl Helper { pub(super) fn new() -> usize { 1 } }\npub(super) const LIMIT: usize = 2;\n",
            ),
            ("external_shadow.rs", "pub fn request() {}\n"),
            (
                "external_consumer.rs",
                "use missing_package::request;\npub fn consume_external() { request(); }\n",
            ),
        ] {
            fs::write(directory.path().join(path), source)
                .unwrap_or_else(|error| panic!("could not write {path}: {error}"));
        }

        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        let facts = ModuleResolverFacts {
            facts: generation.facts(),
        };
        let local_owner = facts.symbol("local.rs", "consume");
        let helper_constructor = facts.symbol("local/helper.rs", "Helper::new");
        let constructor_reference = facts.reference(&local_owner, "Helper::new");
        assert_eq!(
            constructor_reference.target_symbol_id.as_ref(),
            Some(&helper_constructor)
        );
        assert_eq!(
            constructor_reference.resolution_provenance,
            IMPORT_BINDING_PROVENANCE
        );
        for name in ["Helper", "LIMIT"] {
            let target = facts.symbol("local/helper.rs", name);
            let reference = facts.import_declaration_reference("local.rs", name);
            assert_eq!(reference.target_symbol_id.as_ref(), Some(&target), "{name}");
            assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        }

        let external_owner = facts.symbol("external_consumer.rs", "consume_external");
        let external_reference = facts.reference(&external_owner, "request");
        assert!(external_reference.target_symbol_id.is_none());
        assert_eq!(
            external_reference.resolution_provenance,
            RUST_EXTERNAL_UNRESOLVED_PROVENANCE
        );
    }

    #[test]
    fn rust_workspace_crate_imports_resolve_inline_modules_and_named_reexports() {
        let fixtures = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/seam\", \"crates/consumer\"]\nresolver = \"2\"\n",
            ),
            (
                "crates/seam/Cargo.toml",
                "[package]\nname = \"seam\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "crates/seam/src/conv.rs",
                "pub struct Conv2d;\npub fn conv2d() -> Conv2d { Conv2d }\n",
            ),
            (
                "crates/seam/src/lib.rs",
                "pub mod conv;\npub mod nn { pub use crate::conv::{conv2d, Conv2d}; }\npub mod ops { pub fn causal_mask() -> usize { 0 } }\nmod hidden { pub fn secret() {} pub mod visible_inner { pub fn nested_secret() {} } }\n",
            ),
            (
                "crates/consumer/Cargo.toml",
                "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nseam = { path = \"../seam\" }\n",
            ),
            (
                "crates/consumer/src/lib.rs",
                "use seam::nn::{conv2d, Conv2d};\nuse seam::ops::causal_mask;\nuse seam::hidden::secret;\nuse seam::hidden::visible_inner::nested_secret;\npub fn build() -> Conv2d { let _ = causal_mask(); let _ = causal_mask(); conv2d() }\npub fn build_again() -> Conv2d { conv2d() }\npub fn private_boundary() { secret(); nested_secret(); }\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        for facts in [&forward, &reversed] {
            let conv2d = capability_symbol(facts, "crates/seam/src/conv.rs", "conv2d");
            let conv_type = capability_symbol(facts, "crates/seam/src/conv.rs", "Conv2d");
            let mask = capability_symbol(facts, "crates/seam/src/lib.rs", "ops::causal_mask");
            let consumer_file = facts
                .files()
                .iter()
                .find(|file| file.normalized_path == "crates/consumer/src/lib.rs")
                .unwrap_or_else(|| panic!("consumer Rust fixture was missing"));
            let mut matched = 0_usize;
            for reference in facts
                .references()
                .iter()
                .filter(|reference| reference.file_id == consumer_file.file_id)
            {
                let expected = match reference.reference_name.as_str() {
                    "conv2d" => Some(&conv2d.symbol_id),
                    "Conv2d" => Some(&conv_type.symbol_id),
                    "causal_mask" => Some(&mask.symbol_id),
                    _ => None,
                };
                let Some(expected) = expected else {
                    continue;
                };
                matched = matched.saturating_add(1);
                assert_eq!(reference.target_symbol_id.as_ref(), Some(expected));
                assert_eq!(
                    reference.resolution_provenance,
                    RUST_WORKSPACE_CRATE_PROVENANCE
                );
            }
            assert!(matched >= 9, "cross-crate reference sites were missing");
            let private_sites = facts
                .references()
                .iter()
                .filter(|reference| {
                    reference.file_id == consumer_file.file_id
                        && matches!(
                            reference.reference_name.as_str(),
                            "secret" | "nested_secret"
                        )
                })
                .collect::<Vec<_>>();
            assert_ne!(private_sites, [] as [&ReferenceInput; 0]);
            assert!(private_sites.iter().all(|reference| {
                reference.target_symbol_id.is_none()
                    && reference.resolution_provenance == RUST_EXTERNAL_UNRESOLVED_PROVENANCE
            }));
        }
    }

    #[test]
    fn rust_macro_argument_references_resolve_to_cross_module_declarations() {
        let fixtures = [
            (
                "src/lib.rs",
                "mod checks;\nmod limits;\nmod math;\nmod report;\nmod upgrade;\n",
            ),
            (
                "src/upgrade.rs",
                "mod index_child;\nfn whole_minutes(seconds: u64) -> u64 { seconds / 60 }\npub fn timed_out_message() -> String {\n    format!(\"no progress for {} minutes\", whole_minutes(index_child::INDEX_INACTIVITY_TIMEOUT))\n}\n",
            ),
            (
                "src/upgrade/index_child.rs",
                "pub(super) const INDEX_INACTIVITY_TIMEOUT: u64 = 300;\npub(super) const INDEX_UNREAD_TIMEOUT: u64 = 60;\n",
            ),
            (
                "src/math.rs",
                "pub fn compute(value: u32) -> u32 { value * 2 }\npub fn uncalled(value: u32) -> u32 { value }\n",
            ),
            (
                "src/checks.rs",
                "use crate::math::*;\npub fn verify() { assert_eq!(compute(2), 4); }\n",
            ),
            ("src/limits.rs", "pub const ROW_LIMIT: usize = 10;\n"),
            (
                "src/report.rs",
                "use crate::limits::*;\npub fn render() -> String { format!(\"at most {ROW_LIMIT} rows\") }\n",
            ),
        ];
        let forward = build_capability_generation(&fixtures, false);
        let reversed = build_capability_generation(&fixtures, true);
        assert_eq!(forward.digest(), reversed.digest());

        let incoming = |facts: &CanonicalGenerationFacts, target: &SymbolInput| {
            facts
                .edges()
                .iter()
                .filter(|edge| {
                    edge.target_symbol_id == target.symbol_id && edge.kind != EdgeKind::Contains
                })
                .map(|edge| edge.source_symbol_id.clone())
                .collect::<Vec<_>>()
        };
        for facts in [&forward, &reversed] {
            // Each target is used only inside macro arguments, which is what
            // `unused_export` and the call graph previously could not see.
            for (caller_path, caller, target_path, target) in [
                (
                    "src/upgrade.rs",
                    "timed_out_message",
                    "src/upgrade/index_child.rs",
                    "INDEX_INACTIVITY_TIMEOUT",
                ),
                (
                    "src/upgrade.rs",
                    "timed_out_message",
                    "src/upgrade.rs",
                    "whole_minutes",
                ),
                ("src/checks.rs", "verify", "src/math.rs", "compute"),
                ("src/report.rs", "render", "src/limits.rs", "ROW_LIMIT"),
            ] {
                let caller = capability_symbol(facts, caller_path, caller);
                let target = capability_symbol(facts, target_path, target);
                assert_eq!(
                    incoming(facts, target),
                    std::slice::from_ref(&caller.symbol_id),
                    "{target_path}::{}",
                    target.qualified_name
                );
            }
            for (path, name) in [
                ("src/upgrade/index_child.rs", "INDEX_UNREAD_TIMEOUT"),
                ("src/math.rs", "uncalled"),
            ] {
                let unused = capability_symbol(facts, path, name);
                assert_eq!(
                    incoming(facts, unused),
                    [] as [SymbolId; 0],
                    "{path}::{name}"
                );
            }
        }
    }

    #[test]
    fn rust_workspace_crate_resolution_abstains_on_duplicate_package_names() {
        let fixtures = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/seam-a\", \"crates/seam-b\", \"crates/consumer\"]\nresolver = \"2\"\n",
            ),
            (
                "crates/seam-a/Cargo.toml",
                "[package]\nname = \"seam\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "crates/seam-a/src/lib.rs",
                "pub fn public_api() -> usize { 1 }\n",
            ),
            (
                "crates/seam-b/Cargo.toml",
                "[package]\nname = \"seam\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
            ),
            (
                "crates/seam-b/src/lib.rs",
                "pub fn public_api() -> usize { 2 }\n",
            ),
            (
                "crates/consumer/Cargo.toml",
                "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nseam = { path = \"../seam-a\" }\n",
            ),
            (
                "crates/consumer/src/lib.rs",
                "use seam::public_api;\npub fn consume() -> usize { public_api() }\n",
            ),
        ];
        for facts in [
            build_capability_generation(&fixtures, false),
            build_capability_generation(&fixtures, true),
        ] {
            let consumer_file = facts
                .files()
                .iter()
                .find(|file| file.normalized_path == "crates/consumer/src/lib.rs")
                .unwrap_or_else(|| panic!("ambiguous consumer fixture was missing"));
            let sites = facts
                .references()
                .iter()
                .filter(|reference| {
                    reference.file_id == consumer_file.file_id
                        && reference.reference_name == "public_api"
                })
                .collect::<Vec<_>>();
            assert_ne!(sites, [] as [&ReferenceInput; 0]);
            assert!(sites.iter().all(|reference| {
                reference.target_symbol_id.is_none()
                    && reference.resolution_provenance == RUST_EXTERNAL_UNRESOLVED_PROVENANCE
            }));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn javascript_module_stems_prefer_language_extensions_before_directory_indexes() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create module resolver fixture: {error}"));
        write_module_project(directory.path());
        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        let facts = ModuleResolverFacts {
            facts: generation.facts(),
        };
        let owner = facts.symbol("src/ambiguous.ts", "useAmbiguous");
        let reference = facts.reference(&owner, "choose");
        let target = facts.symbol("src/foo.ts", "choose");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bare_calls_do_not_resolve_to_sibling_class_methods() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create module resolver fixture: {error}"));
        write_module_project(directory.path());
        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        let facts = ModuleResolverFacts {
            facts: generation.facts(),
        };
        let execute = facts.symbol("src/members.ts", "Worker::execute");
        let outer_run = facts.symbol("src/members.ts", "run");
        let reference = facts.reference(&execute, "run");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&outer_run));
        assert_eq!(reference.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);
    }

    #[test]
    fn relative_module_paths_are_project_bounded_and_declaration_extensions_are_atomic() {
        assert_eq!(
            normalize_relative_module_path("src/nested/consumer.ts", "../api"),
            Some("src/api".to_owned())
        );
        assert_eq!(
            normalize_relative_module_path("consumer.ts", "./api"),
            Some("api".to_owned())
        );
        assert_eq!(
            normalize_relative_module_path("consumer.ts", "../api"),
            None
        );
        assert_eq!(
            normalize_relative_module_path("src/nested/consumer.ts", ".."),
            Some("src".to_owned())
        );
        assert_eq!(
            normalize_relative_module_path("src/consumer.ts", "package"),
            None
        );
        assert_eq!(
            strip_module_extension("src/api.d.ts", SourceLanguage::TypeScript.as_str()),
            Some("src/api")
        );
        assert_eq!(
            strip_module_extension("src/api.d.mts", SourceLanguage::TypeScript.as_str()),
            Some("src/api")
        );
        assert_eq!(
            strip_module_extension("scripts/helper.wren", SourceLanguage::Wren.as_str()),
            Some("scripts/helper")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_qualified_members_remain_unresolved_and_sensitive_literals_are_redacted() {
        const SECRET_LITERALS: [&str; SECRET_LITERAL_COUNT] = [
            INITIALIZER_SECRET,
            SCALAR_DEFAULT_SECRET,
            DESTRUCTURED_DEFAULT_SECRET,
            ARROW_DEFAULT_SECRET,
            METHOD_DEFAULT_SECRET,
            COMPONENT_DEFAULT_SECRET,
        ];
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create resolver fixture: {error}"),
        };
        assert!(
            fs::write(
                directory.path().join("local.ts"),
                format!(
                    "export function log(): void {{}}\n\
                     export class A {{ log(): void {{}} }}\n\
                     export class B {{ log(): void {{}} }}\n\
                     export function local(): void {{ console.log('x'); }}\n\
                     export const credentials = {{ token: '{INITIALIZER_SECRET}' }};\n\
                     export function scalar(token = '{SCALAR_DEFAULT_SECRET}'): void {{}}\n\
                     export function destructured({{ token = '{DESTRUCTURED_DEFAULT_SECRET}' }} = {{}}): void {{}}\n\
                     export const arrow = (token = '{ARROW_DEFAULT_SECRET}'): void => {{}};\n\
                     export class Client {{ method(token = '{METHOD_DEFAULT_SECRET}'): void {{}} }}\n\
                     export function Component({{ token = '{COMPONENT_DEFAULT_SECRET}' }} = {{}}): null {{ return null; }}\n",
                ),
            )
            .is_ok()
        );
        assert!(
            fs::write(
                directory.path().join("remote.ts"),
                "export function remote(): void { console.log('y'); }\n",
            )
            .is_ok()
        );

        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        let console_references = generation
            .facts()
            .references()
            .iter()
            .filter(|reference| reference.reference_name == "console.log")
            .collect::<Vec<_>>();
        assert_eq!(console_references.len(), 2);
        assert!(
            console_references.iter().all(|reference| {
                reference.target_symbol_id.is_none()
                    && reference.owner_symbol_id.is_some()
                    && reference.confidence == UNRESOLVED_CONFIDENCE
                    && reference.resolution_provenance == JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE
            }),
            "unexpected console references: {console_references:#?}"
        );
        assert!(
            generation
                .facts()
                .symbols()
                .iter()
                .filter(|symbol| symbol.qualified_name.ends_with("credentials"))
                .all(|symbol| symbol.signature.is_empty())
        );
        for secret in SECRET_LITERALS {
            assert!(
                generation
                    .facts()
                    .symbols()
                    .iter()
                    .all(|symbol| !symbol.signature.contains(secret))
            );
            assert!(generation.facts().documents().iter().all(|document| {
                !document.code().contains(secret)
                    && !document.natural_text().contains(secret)
                    && !document.metadata_json().contains(secret)
            }));
        }
    }

    #[test]
    fn clone_prefilter_is_an_exact_comparison_upper_bound() {
        let left = [
            CloneTokenCount(0x0000_0000_0000_0001, 4),
            CloneTokenCount(0x1000_0000_0000_0001, 6),
        ];
        let right = [
            CloneTokenCount(0x0000_0000_0000_0001, 2),
            CloneTokenCount(0x0000_0000_0000_0002, 8),
        ];
        let exact = clone_multiset_overlap_ppm(
            CloneProfileView {
                counts: &left,
                total: 10,
            },
            CloneProfileView {
                counts: &right,
                total: 10,
            },
        );
        let prefiltered = clone_prefilter_overlap_ppm(
            ClonePrefilter {
                buckets: &clone_count_prefilter(&left),
                total: 10,
            },
            ClonePrefilter {
                buckets: &clone_count_prefilter(&right),
                total: 10,
            },
        );
        assert_eq!(exact, 200_000);
        assert_eq!(prefiltered, 400_000);
        assert!(prefiltered >= exact);

        let disjoint = [CloneTokenCount(0xf000_0000_0000_0001, 10)];
        assert_eq!(
            clone_prefilter_overlap_ppm(
                ClonePrefilter {
                    buckets: &clone_count_prefilter(&left),
                    total: 10,
                },
                ClonePrefilter {
                    buckets: &clone_count_prefilter(&disjoint),
                    total: 10,
                },
            ),
            0
        );
    }

    #[test]
    fn compact_clone_accounting_excludes_unmaterialized_canonical_output() {
        let build_compact = || {
            let limits = SourceLimits::new(TEST_SOURCE_BYTES)
                .unwrap_or_else(|error| panic!("source limits were invalid: {error}"));
            let snapshot = cartograph_extract::SourceSnapshot::from_bytes(
                "compact.ts",
                b"export function alpha(): void { beta(); }\nexport function beta(): void {}\n",
                limits,
            )
            .unwrap_or_else(|error| panic!("compact snapshot was rejected: {error}"));
            let mut extractor = NativeExtractor::new(snapshot.language())
                .unwrap_or_else(|error| panic!("native extractor was unavailable: {error}"));
            let extracted = extractor
                .extract(&snapshot)
                .unwrap_or_else(|error| panic!("compact extraction failed: {error}"));
            let file = NativeFileFacts::from_extracted(extracted)
                .unwrap_or_else(|_| panic!("compact facts could not be normalized"));
            compact_clone_file(file)
        };
        let compact = build_compact();
        let owned = compact.modeled_owned_bytes();
        assert!(compact.modeled_retained_bytes() > owned);
        let maximum = owned.saturating_add(usize_to_u64(size_of::<NativeFileFacts>()));
        let mut clone_accumulator = NativeFactAccumulator::new(maximum);
        clone_accumulator
            .push_compact_clone(compact, 0)
            .unwrap_or_else(|_| panic!("retained compact clone bytes were overcharged"));
        let mut canonical_accumulator = NativeFactAccumulator::new(maximum);
        assert!(
            canonical_accumulator
                .push_native(build_compact(), 0)
                .is_err()
        );
    }

    #[test]
    fn long_paths_and_many_symbols_stay_inside_resolve_and_validation_charges() {
        let component = "p".repeat(LONG_PATH_COMPONENT_BYTES);
        let directory = std::iter::repeat_n(component.as_str(), LONG_PATH_COMPONENT_COUNT)
            .collect::<Vec<_>>()
            .join("/");
        let path = format!("{directory}/many.ts");
        assert!(path.len() > MINIMUM_LONG_PATH_BYTES);
        let mut source = String::new();
        for index in 0..MANY_SYMBOL_COUNT {
            append_fixture_text(
                &mut source,
                format_args!("export function symbol_{index}(): void {{}}\n"),
            );
        }
        let limits = SourceLimits::new(TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("source limits were invalid: {error}"));
        let snapshot =
            cartograph_extract::SourceSnapshot::from_bytes(&path, source.as_bytes(), limits)
                .unwrap_or_else(|error| panic!("long-path snapshot was rejected: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("native extractor was unavailable: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("long-path extraction failed: {error}"));
        let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
        accumulator
            .push(extracted)
            .unwrap_or_else(|_| panic!("long-path facts exceeded the modeled input limit"));
        let (facts, resolve) = resolve_generation(
            ResolveGenerationRequest {
                extracted: accumulator,
                maximum_bytes: TEST_GENERATION_BYTES,
                source_root: test_source_root(),
                evidence_policy: FULL_TEST_EVIDENCE,
                clone_policy: NativeClonePolicy {
                    wider_partial_band: false,
                },
            },
            || false,
        )
        .unwrap_or_else(|_| panic!("long-path resolution exceeded its declared budget"));
        assert!(
            resolve.charged_high_water_bytes
                <= resolve_reservation(TEST_GENERATION_BYTES)
                    .unwrap_or_else(|error| panic!("resolve reservation failed: {error}"))
        );
        let validation_limits =
            generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
                .unwrap_or_else(|error| panic!("validation limits failed: {error}"));
        let (canonical, validation) = validate_generation_facts(facts, validation_limits, || false)
            .unwrap_or_else(|error| panic!("long-path validation failed: {error}"));
        assert!(validation.charged_high_water_bytes() <= validation_limits.maximum_working_bytes());
        let file_document = canonical
            .documents()
            .iter()
            .find(|document| {
                document.path() == path
                    && document.qualified_name().is_empty()
                    && document.code() == path
            })
            .unwrap_or_else(|| panic!("file search document was missing"));
        assert_eq!(file_document.qualified_name(), "");
        assert_eq!(file_document.code(), path);
        let file_symbol_id = file_document
            .symbol_id()
            .unwrap_or_else(|| panic!("file search document was not graph-addressable"));
        let file_symbol = canonical
            .symbols()
            .iter()
            .find(|symbol| &symbol.symbol_id == file_symbol_id)
            .unwrap_or_else(|| panic!("file graph symbol was missing"));
        assert!(
            file_symbol
                .qualified_name
                .starts_with(FILE_SYMBOL_FALLBACK_PREFIX)
        );
        assert!(file_symbol.qualified_name.len() <= MAX_SYMBOL_QUALIFIED_NAME_BYTES);
    }

    #[test]
    fn resolve_polls_cancellation_between_candidate_symbols() {
        let mut source = String::new();
        for index in 0..CANCELLATION_SYMBOL_COUNT {
            append_fixture_text(
                &mut source,
                format_args!("export class Symbol{index} {{}}\n"),
            );
        }
        let limits = SourceLimits::new(TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("source limits were invalid: {error}"));
        let snapshot =
            cartograph_extract::SourceSnapshot::from_bytes("many.ts", source.as_bytes(), limits)
                .unwrap_or_else(|error| panic!("cancellation snapshot failed: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("native extractor was unavailable: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("cancellation extraction failed: {error}"));
        let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
        accumulator
            .push(extracted)
            .unwrap_or_else(|_| panic!("cancellation facts exceeded the modeled input limit"));
        let polls = Cell::new(0_u64);
        let result = resolve_generation(
            ResolveGenerationRequest {
                extracted: accumulator,
                maximum_bytes: TEST_GENERATION_BYTES,
                source_root: test_source_root(),
                evidence_policy: FULL_TEST_EVIDENCE,
                clone_policy: NativeClonePolicy {
                    wider_partial_band: false,
                },
            },
            || {
                let next = polls.get().saturating_add(1);
                polls.set(next);
                next >= CANCEL_AFTER_POLLS
            },
        );
        assert_matches!(result, Err(ResolveGenerationFailure { reason: None }));
        assert_eq!(polls.get(), CANCEL_AFTER_POLLS);
    }

    #[test]
    fn candidate_selection_polls_cancellation_within_one_reference() {
        let file_id = FileId::from_uuid_v8([TEST_FILE_ID_BYTE; DOCUMENT_UUID_BYTES]);
        let candidates = (0..INNER_CANCELLATION_CANDIDATE_COUNT)
            .map(|index| {
                let mut symbol_bytes = [TEST_SYMBOL_ID_BYTE; DOCUMENT_UUID_BYTES];
                symbol_bytes[DOCUMENT_UUID_BYTES - 1] = index;
                ResolutionCandidate {
                    file_id: file_id.clone(),
                    symbol_id: SymbolId::from_uuid_v8(symbol_bytes),
                    parent_symbol_id: None,
                    qualified_name: format!("candidate_{index}"),
                    signature: String::new(),
                    kind: SymbolKind::Function,
                    visibility: None,
                    implementation: SymbolImplementationFlags::default(),
                    export: SymbolExportFlags::named(true),
                    top_level: true,
                    augmentation: false,
                    static_member: false,
                    declaration_syntax: DeclarationSyntax::Other,
                    declaration_span: (0, 0),
                }
            })
            .collect::<Vec<_>>();
        let polls = Cell::new(0_u64);
        let result = select_candidate(&candidates, |_| true, &mut || {
            let next = polls.get().saturating_add(1);
            polls.set(next);
            next >= INNER_CANCEL_AFTER_POLLS
        });
        assert_matches!(result.err(), Some(StageItemFailure));
        assert_eq!(polls.get(), INNER_CANCEL_AFTER_POLLS);
    }

    fn resolve_unresolved_reference_fixture(
        maximum_bytes: u64,
    ) -> (GenerationFacts, ResolutionReport) {
        let mut calls = String::new();
        for index in 0..256 {
            append_fixture_text(&mut calls, format_args!("missing_{index}();"));
        }
        let source = format!("export function owner(): void {{ {calls} }}\n");
        let limits = SourceLimits::new(TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("source limits were invalid: {error}"));
        let snapshot = cartograph_extract::SourceSnapshot::from_bytes(
            "unresolved.ts",
            source.as_bytes(),
            limits,
        )
        .unwrap_or_else(|error| panic!("unresolved snapshot failed: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("native extractor was unavailable: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("unresolved extraction failed: {error}"));
        // Parsing has its own retained-output admission. This helper varies
        // only the resolver's final canonical-output policy.
        let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
        accumulator
            .push(extracted)
            .unwrap_or_else(|_| panic!("unresolved facts exceeded the modeled input limit"));
        resolve_generation(
            ResolveGenerationRequest {
                extracted: accumulator,
                maximum_bytes,
                source_root: test_source_root(),
                evidence_policy: FULL_TEST_EVIDENCE,
                clone_policy: NativeClonePolicy {
                    wider_partial_band: false,
                },
            },
            || false,
        )
        .unwrap_or_else(|_| panic!("unresolved resolution exceeded its declared budget"))
    }

    #[test]
    fn unresolved_reference_edge_capacity_is_part_of_the_retained_proof() {
        let (facts, report) = resolve_unresolved_reference_fixture(TEST_GENERATION_BYTES);
        assert_eq!(report.resolved, 0);
        assert_eq!(report.unresolved, 256);
        assert_eq!(facts.edges.len(), 1);
        assert!(
            facts
                .edges
                .iter()
                .all(|edge| edge.kind == EdgeKind::Contains)
        );
        assert!(facts.edges.capacity() >= facts.references.len());
        let edge_capacity_bytes = usize_to_u64(facts.edges.capacity())
            .saturating_mul(usize_to_u64(size_of::<EdgeInput>()));
        assert!(report.retained_bytes >= edge_capacity_bytes);
    }

    #[test]
    fn resolve_keeps_unordered_working_facts_distinct_from_canonical_output() {
        let (facts, report) = resolve_unresolved_reference_fixture(TEST_GENERATION_BYTES);
        let generous_limits =
            generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
                .unwrap_or_else(|error| panic!("generous validation limits failed: {error}"));
        let (_, validation) = validate_generation_facts(facts, generous_limits, || false)
            .unwrap_or_else(|error| panic!("generous validation failed: {error}"));
        assert!(report.retained_bytes > validation.output_bytes());
        let maximum_output_bytes = report
            .retained_bytes
            .checked_sub(1)
            .unwrap_or_else(|| panic!("fixture output limit underflowed"));
        assert!(validation.output_bytes() < maximum_output_bytes);
        assert!(maximum_output_bytes < report.retained_bytes);

        let (facts, bounded_report) = resolve_unresolved_reference_fixture(maximum_output_bytes);
        assert!(bounded_report.retained_bytes > maximum_output_bytes);
        let bounded_limits =
            generation_validation_limits(maximum_output_bytes, PipelineStage::Reduce)
                .unwrap_or_else(|error| panic!("bounded validation limits failed: {error}"));
        let (_, bounded_validation) = validate_generation_facts(facts, bounded_limits, || false)
            .unwrap_or_else(|error| panic!("bounded validation failed: {error}"));
        assert!(bounded_validation.output_bytes() <= maximum_output_bytes);
        assert!(
            bounded_validation.charged_high_water_bytes() <= bounded_limits.maximum_working_bytes()
        );
    }

    #[test]
    fn resolve_measurement_classifies_only_the_retained_limit_as_capacity() {
        assert_eq!(
            classify_resolve_measurement_failure(GenerationMemoryModelError::RetainedLimit)
                .reason(),
            Some(PipelineFailureReason::GenerationCapacityExceeded)
        );
        for error in [
            GenerationMemoryModelError::Cancelled,
            GenerationMemoryModelError::MetadataDepth,
        ] {
            assert_eq!(classify_resolve_measurement_failure(error).reason(), None);
        }
    }

    #[test]
    fn graph_policy_can_omit_docstrings_and_call_sites_without_losing_edges() {
        let source = "/// Sensitive implementation notes.\npub fn target() {}\npub fn caller() { target(); }\n";
        let limits = SourceLimits::new(TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("source limits were invalid: {error}"));
        let snapshot =
            cartograph_extract::SourceSnapshot::from_bytes("policy.rs", source.as_bytes(), limits)
                .unwrap_or_else(|error| panic!("policy snapshot failed: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("native extractor was unavailable: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("policy extraction failed: {error}"));
        let mut accumulator = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
        accumulator
            .push(extracted)
            .unwrap_or_else(|_| panic!("policy facts exceeded the modeled input limit"));
        let (facts, _) = resolve_generation(
            ResolveGenerationRequest {
                extracted: accumulator,
                maximum_bytes: TEST_GENERATION_BYTES,
                source_root: test_source_root(),
                evidence_policy: STRUCTURAL_TEST_EVIDENCE,
                clone_policy: NativeClonePolicy {
                    wider_partial_band: false,
                },
            },
            || false,
        )
        .unwrap_or_else(|_| panic!("policy resolution failed"));
        assert_eq!(facts.references, []);
        assert!(facts.edges.iter().any(|edge| edge.kind == EdgeKind::Calls));
        assert!(facts.documents.iter().all(|document| {
            document.natural_text.is_empty()
                || !document
                    .natural_text
                    .contains("Sensitive implementation notes")
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_rejects_storage_oversized_names_and_signatures_before_returning() {
        // A synthesized name past its bound is shortened rather than fatal, so
        // one ordinary construct cannot cost the whole index (issue #119).
        let long_name = "n".repeat(2_049);
        assert_oversized_name_is_shortened(format!("export function {long_name}(): void {{}}\n"))
            .await;

        // A field with no safe shortening still fails, and now names itself.
        let long_type = "T".repeat(65_537);
        assert_storage_boundary_rejection(format!(
            "export function bounded(value: {long_type}): void {{}}\n"
        ))
        .await;
    }

    async fn assert_oversized_name_is_shortened(source: String) {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create storage-boundary fixture: {error}"));
        assert!(fs::write(directory.path().join("oversized.ts"), source).is_ok());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open storage-boundary fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let generation = build_native_generation(&runner, source_root, config(SERIAL_WORKERS))
            .await
            .unwrap_or_else(|error| panic!("an over-long name must not be fatal: {error}"));
        assert!(
            generation
                .facts
                .symbols()
                .iter()
                .all(|symbol| symbol.qualified_name.len() <= 2_048),
            "every stored qualified name must fit the canonical bound"
        );
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
    }

    async fn assert_storage_boundary_rejection(source: String) {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create storage-boundary fixture: {error}"));
        assert!(fs::write(directory.path().join("oversized.ts"), source).is_ok());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open storage-boundary fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let result = build_native_generation(&runner, source_root, config(SERIAL_WORKERS)).await;
        let Err(NativePipelineError::Validation { reason }) = result else {
            panic!("a source past a storage boundary must fail canonical validation");
        };
        // Naming the rejected field is what turns a whole-corpus bisection into
        // a single run (issue #118).
        assert!(
            reason.is_some_and(|reason| reason.rejected_field().is_some()),
            "a rejected bounded field must be named, not reported as a bare stage failure"
        );
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_reference_names_are_shortened_without_retaining_source_text() {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create reference-boundary fixture: {error}"));
        let long_target = "reference_target_".repeat(300);
        let source = format!("pub fn trigger() {{ {long_target}(); }}\n");
        assert!(fs::write(directory.path().join("oversized.rs"), source).is_ok());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open reference-boundary fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let result = build_native_generation(&runner, source_root, config(SERIAL_WORKERS)).await;
        let facts = result.unwrap_or_else(|error| {
            panic!("one over-long synthesized name must not discard the generation: {error}")
        });
        assert!(
            facts
                .facts
                .references()
                .iter()
                .all(|reference| reference.reference_name.len() <= 4_096),
            "every stored reference name must fit the canonical bound"
        );
        assert!(
            !facts
                .facts
                .references()
                .iter()
                .any(|reference| reference.reference_name == long_target),
            "the unbounded original name must not be stored verbatim"
        );
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(!report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parse_refuses_content_drift_after_the_read_manifest() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create pipeline fixture: {error}"),
        };
        assert!(
            fs::write(
                directory.path().join("drift.ts"),
                "export const before = 1;\n"
            )
            .is_ok()
        );
        let source_root = match SourceRoot::open(directory.path()) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open pipeline fixture: {error}"),
        };
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let stages = NativeStageContext {
            runner: &runner,
            source_root,
            config: config(SERIAL_WORKERS),
        };
        let (discovered, _) = match run_discovery_stage(&stages).await {
            Ok(discovered) => discovered,
            Err(error) => panic!("discovery failed: {error}"),
        };
        let manifest = match run_read_stage(&stages, discovered).await {
            Ok((manifest, _)) => manifest,
            Err(error) => panic!("read stage failed: {error}"),
        };
        assert!(
            fs::write(
                directory.path().join("drift.ts"),
                "export const afterx = 2;\n"
            )
            .is_ok()
        );
        let Err(error) = run_parse_stage(&stages, manifest.entries, None).await else {
            panic!("content drift must fail the parse stage");
        };
        let failure = error
            .file_failure()
            .unwrap_or_else(|| panic!("parse failure omitted its project-relative input: {error}"));
        assert_eq!(failure.path().as_str(), "drift.ts");
        assert_eq!(
            failure.reason(),
            PipelineFailureReason::SourceChangedDuringParse
        );
        let rendered = error.to_string();
        assert!(rendered.contains("source_changed_during_parse"));
        assert!(rendered.contains("\"drift.ts\""));
        assert!(!rendered.contains(&directory.path().to_string_lossy().into_owned()));
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[test]
    fn spilled_parse_item_deadline_does_not_reuse_retained_file_failure() {
        let sequence = StageSequence::new(7);
        let path = NormalizedPath::parse("src/innocent.rs")
            .unwrap_or_else(|error| panic!("deadline fixture path was invalid: {error}"));
        let failures = Mutex::new(BTreeMap::from([(
            sequence,
            ParseManifestFailure::from_extract(ExtractError::Cancelled).with_path(path),
        )]));

        let error = classify_spilled_parse_stage_error(
            StageRunError::Item {
                stage: PipelineStage::Parse,
                sequence,
                kind: StageFailureKind::Deadline,
            },
            &failures,
        );

        assert_eq!(
            error.reason(),
            Some(PipelineFailureReason::DeadlineExceeded)
        );
        assert!(error.file_failure().is_none());
        assert_matches!(
            error,
            NativePipelineError::Stage(StageRunError::Item {
                stage: PipelineStage::Parse,
                kind: StageFailureKind::Deadline,
                ..
            })
        );
    }

    #[test]
    fn spilled_parse_storage_failure_stays_stage_scoped() {
        let path = NormalizedPath::parse("src/not-at-fault.rs")
            .unwrap_or_else(|error| panic!("spill fixture path was invalid: {error}"));
        let failure = ParseManifestFailure::from_spill(&StorageError::DatabaseOperation {
            operation: "spill-test",
        })
        .with_path(path);

        let error = classify_parse_stage_failure(
            StageRunError::Item {
                stage: PipelineStage::Parse,
                sequence: StageSequence::new(0),
                kind: StageFailureKind::Worker,
            },
            Some(failure),
        );

        assert_matches!(
            error,
            NativePipelineError::Spill {
                stage: PipelineStage::Parse
            }
        );
        assert!(error.file_failure().is_none());
        assert_eq!(error.reason(), None);
    }

    /// The spilled reduce runs as one stage item, so the runner only learns
    /// that the item failed. A spill statement that outlived its timeout used
    /// to surface as a reason-less `reduce_failed`; it is the reduce deadline.
    #[test]
    fn spilled_reduce_statement_timeout_reports_the_reduce_deadline() {
        let worker_failure = || StageRunError::Item {
            stage: PipelineStage::Reduce,
            sequence: StageSequence::new(0),
            kind: StageFailureKind::Worker,
        };
        let timed_out = SpilledReduceFailure::default();
        let _ = timed_out.record(StorageError::StatementTimeout {
            operation: "spill-detect-canonical-conflict",
        });
        assert_matches!(
            timed_out.classify(worker_failure()),
            NativePipelineError::StageWithReason {
                stage: PipelineStage::Reduce,
                reason: PipelineFailureReason::DeadlineExceeded,
            }
        );

        let failed = SpilledReduceFailure::default();
        let _ = failed.record(StorageError::GenerationSpillConflict);
        let error = failed.classify(worker_failure());
        assert_matches!(
            error,
            NativePipelineError::Spill {
                stage: PipelineStage::Reduce
            }
        );
        assert_eq!(error.reason(), None);

        let unrecorded = SpilledReduceFailure::default().classify(worker_failure());
        assert_matches!(
            unrecorded,
            NativePipelineError::Stage(StageRunError::Item {
                stage: PipelineStage::Reduce,
                kind: StageFailureKind::Worker,
                ..
            })
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cache_hit_revalidation_preserves_typed_file_reasons() {
        const ORIGINAL: &str = "pub fn cached_probe() -> u32 { 1 }\n";
        const CHANGED: &str = "pub fn cached_probe() -> u32 { 2 }\n";
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create cache fixture: {error}"));
        let path = directory.path().join("cached.rs");
        fs::write(&path, ORIGINAL)
            .unwrap_or_else(|error| panic!("could not write cache fixture: {error}"));
        let limits = SourceLimits::new(TEST_SOURCE_BYTES)
            .unwrap_or_else(|error| panic!("cache fixture limits were invalid: {error}"));
        let snapshot = SourceSnapshot::from_bytes("cached.rs", ORIGINAL.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("cache fixture snapshot failed: {error}"));
        let manifest = SourceManifestEntry {
            path: snapshot.path().clone(),
            language: snapshot.language(),
            file_id: snapshot.file_id().clone(),
            content_hash: snapshot.content_hash().clone(),
            byte_size: snapshot.byte_size(),
        };
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open cache fixture: {error}"));

        fs::write(&path, CHANGED)
            .unwrap_or_else(|error| panic!("could not mutate cache fixture: {error}"));
        assert_revalidation_failure(
            revalidate_manifest(&source_root, &manifest, || false),
            PipelineFailureReason::SourceChangedDuringParse,
        );

        fs::remove_file(&path)
            .unwrap_or_else(|error| panic!("could not remove cache fixture: {error}"));
        assert_revalidation_failure(
            revalidate_manifest(&source_root, &manifest, || false),
            PipelineFailureReason::SourceReadFailed,
        );

        fs::write(&path, ORIGINAL)
            .unwrap_or_else(|error| panic!("could not restore cache fixture: {error}"));
        assert_revalidation_failure(
            revalidate_manifest(&source_root, &manifest, || true),
            PipelineFailureReason::ExtractionCancelled,
        );
    }

    fn assert_revalidation_failure(
        result: Result<(), ParseManifestFailure>,
        expected: PipelineFailureReason,
    ) {
        let Err(failure) = result else {
            panic!("cache revalidation unexpectedly passed");
        };
        let file = failure
            .file_failure()
            .unwrap_or_else(|| panic!("cache revalidation omitted its file diagnostic"));
        assert_eq!(file.path().as_str(), "cached.rs");
        assert_eq!(file.reason(), expected);
    }

    #[test]
    fn parser_failures_retain_exact_allowlisted_reasons() {
        let cases = [
            (
                ExtractError::UnsupportedLanguage,
                PipelineFailureReason::ExtractionUnsupportedLanguage,
            ),
            (
                ExtractError::LanguageMismatch,
                PipelineFailureReason::ExtractionLanguageMismatch,
            ),
            (
                ExtractError::GrammarUnavailable,
                PipelineFailureReason::ExtractionGrammarUnavailable,
            ),
            (
                ExtractError::ParserStopped,
                PipelineFailureReason::ExtractionParserStopped,
            ),
            (
                ExtractError::Cancelled,
                PipelineFailureReason::ExtractionCancelled,
            ),
            (
                ExtractError::InvalidSpan,
                PipelineFailureReason::ExtractionInvalidSpan,
            ),
            (
                ExtractError::NestingLimit,
                PipelineFailureReason::ExtractionNestingLimitExceeded,
            ),
            (
                ExtractError::InvalidNestingLimit,
                PipelineFailureReason::ExtractionInvalidNestingLimit,
            ),
            (
                ExtractError::OutputLimit,
                PipelineFailureReason::ExtractionOutputLimitExceeded,
            ),
        ];
        for (error, expected) in cases {
            let path = NormalizedPath::parse("src/failing.rs")
                .unwrap_or_else(|invalid| panic!("fixture path was invalid: {invalid}"));
            let failure = ParseManifestFailure::from_extract(error).with_path(path);
            let file = failure
                .file_failure()
                .unwrap_or_else(|| panic!("{error} omitted its file diagnostic"));
            assert_eq!(file.path().as_str(), "src/failing.rs");
            assert_eq!(file.reason(), expected);
            assert_ne!(expected.description(), "");
        }
    }

    #[test]
    fn recoverable_parser_failures_keep_stable_degraded_reasons() {
        assert_eq!(
            degraded_file_reason(DiagnosticCode::InvalidSpan),
            Some("extraction_invalid_span")
        );
        assert_eq!(
            degraded_file_reason(DiagnosticCode::ParserStopped),
            Some("extraction_parser_stopped")
        );
        assert_eq!(
            degraded_file_reason(DiagnosticCode::NestingLimitExceeded),
            Some("nesting_limit_exceeded")
        );
        assert_eq!(degraded_file_reason(DiagnosticCode::SyntaxError), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parse_recovers_nesting_limits_and_preserves_fatal_output_limit_reasons() {
        let mut nested = String::from("void deep(void) {\n");
        for _ in 0..300 {
            nested.push_str("if (enabled) {\n");
        }
        for _ in 0..300 {
            nested.push_str("}\n");
        }
        nested.push_str("}\n");
        assert_parse_nesting_recovery("private-nesting-marker.c", nested.clone()).await;
        assert_parse_nesting_override("private-nesting-marker.c", nested).await;

        let mut excessive = String::new();
        for index in 0..20_000 {
            excessive.push_str("int v");
            excessive.push_str(&index.to_string());
            excessive.push_str(";\n");
        }
        assert_parse_extraction_reason(
            "private-output-marker.c",
            excessive,
            PipelineFailureReason::ExtractionOutputLimitExceeded,
        )
        .await;
    }

    async fn assert_parse_nesting_recovery(file_name: &str, source: String) {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create nesting-limit fixture: {error}"));
        assert!(fs::write(directory.path().join(file_name), &source).is_ok());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open nesting-limit fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let generation = build_native_generation(&runner, source_root, config(SERIAL_WORKERS))
            .await
            .unwrap_or_else(|error| panic!("nesting-limit fixture did not recover: {error}"));
        let report = generation.report();
        assert_eq!(report.degraded_files_truncated(), 0);
        assert_eq!(report.degraded_files().len(), 1);
        assert_eq!(report.degraded_files()[0].path(), file_name);
        assert_eq!(
            report.degraded_files()[0].reason(),
            "nesting_limit_exceeded"
        );
        drop(cancellation);
        let task_report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(task_report.all_joined);
        assert!(!task_report.worker_failed);
    }

    async fn assert_parse_nesting_override(file_name: &str, source: String) {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create nesting override fixture: {error}"));
        assert!(fs::write(directory.path().join(file_name), &source).is_ok());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open nesting override fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let pipeline = config(SERIAL_WORKERS)
            .with_maximum_ast_depth(1_024)
            .unwrap_or_else(|error| panic!("nesting override was invalid: {error}"));
        let generation = build_native_generation(&runner, source_root, pipeline)
            .await
            .unwrap_or_else(|error| panic!("nesting override did not parse: {error}"));
        assert_eq!(generation.report().degraded_files(), []);
        assert!(
            generation
                .facts()
                .symbols()
                .iter()
                .any(|symbol| symbol.qualified_name == "deep")
        );
        drop(cancellation);
        let task_report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(task_report.all_joined);
        assert!(!task_report.worker_failed);
    }

    async fn assert_parse_extraction_reason(
        file_name: &str,
        source: String,
        expected: PipelineFailureReason,
    ) {
        let directory = tempdir()
            .unwrap_or_else(|error| panic!("could not create extraction-limit fixture: {error}"));
        assert!(fs::write(directory.path().join(file_name), &source).is_ok());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open extraction-limit fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let result = build_native_generation(&runner, source_root, config(SERIAL_WORKERS)).await;
        let Err(error) = result else {
            panic!("extraction-limit fixture unexpectedly produced a generation");
        };
        assert_eq!(error.stage(), PipelineStage::Parse);
        assert_eq!(error.reason(), Some(expected));
        let failure = error
            .file_failure()
            .unwrap_or_else(|| panic!("extraction failure omitted its relative input path"));
        assert_eq!(failure.path().as_str(), file_name);
        assert_eq!(failure.reason(), expected);
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains(file_name));
        assert!(!rendered.contains("private_output_marker"));
        assert!(!rendered.contains(&directory.path().to_string_lossy().to_string()));
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_failure_preserves_the_real_native_stage() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create resolve fixture: {error}"));
        write_project(directory.path());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open resolve fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let stages = NativeStageContext {
            runner: &runner,
            source_root: source_root.clone(),
            config: config(SERIAL_WORKERS),
        };
        let extracted = parse_project_through_native_stages(&stages).await;
        let rejecting = NativeStageContext {
            runner: &runner,
            source_root,
            config: config_with_generation_limit(SERIAL_WORKERS, REJECTING_GENERATION_BYTES),
        };
        let Err(error) = run_resolve_stage(&rejecting, extracted).await else {
            panic!("rejecting resolve limit unexpectedly produced facts");
        };
        assert_eq!(error.stage(), PipelineStage::Resolve);
        assert_eq!(
            error.reason(),
            Some(PipelineFailureReason::GenerationCapacityExceeded)
        );
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overlay_failure_preserves_the_real_native_stage() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("could not create overlay fixture: {error}"));
        write_project(directory.path());
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("could not open overlay fixture: {error}"));
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let stages = NativeStageContext {
            runner: &runner,
            source_root,
            config: config(SERIAL_WORKERS),
        };
        let extracted = parse_project_through_native_stages(&stages).await;
        let (facts, _) = run_resolve_stage(&stages, extracted)
            .await
            .unwrap_or_else(|error| panic!("resolve stage failed: {error}"));
        let overlay = ScipOverlayInput::new(vec![0xff], 1)
            .unwrap_or_else(|error| panic!("invalid overlay test setup: {error}"));
        let Err(error) = run_scip_overlay_stage(&stages, facts, overlay).await else {
            panic!("malformed overlay unexpectedly produced facts");
        };
        assert_eq!(error.stage(), PipelineStage::Overlay);
        assert_matches!(
            error,
            NativePipelineError::Stage(StageRunError::Item {
                stage: PipelineStage::Overlay,
                ..
            })
        );
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retained_generation_limit_fails_before_unbounded_accumulation() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create pipeline fixture: {error}"),
        };
        write_project(directory.path());
        let source_root = match SourceRoot::open(directory.path()) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open pipeline fixture: {error}"),
        };
        let (runner, tasks, cancellation) =
            test_stage_runner(DRIFT_SCOPE_TASKS, TEST_SCOPE_BYTES).await;
        let result = build_native_generation(
            &runner,
            source_root,
            config_with_generation_limit(SERIAL_WORKERS, REJECTING_GENERATION_BYTES),
        )
        .await;
        assert_matches!(
            result,
            Err(NativePipelineError::StageWithReason {
                stage: PipelineStage::Parse,
                reason: PipelineFailureReason::GenerationCapacityExceeded,
            })
        );
        drop(cancellation);
        let report = tasks
            .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
            .await;
        assert!(report.all_joined);
        assert!(report.worker_failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn polyglot_and_module_forms_produce_resolved_nonempty_graphs() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create polyglot pipeline fixture: {error}"),
        };
        write_polyglot_project(directory.path());
        let generation = build(directory.path(), PARALLEL_WORKERS).await;
        let facts = ModuleResolverFacts {
            facts: generation.facts(),
        };

        assert_polyglot_languages(generation.facts());
        assert_polyglot_resolved_references(&facts);

        assert_polyglot_export_edges(&facts);
        assert_polyglot_rejections(&facts);
        assert_polyglot_files_are_nonempty(generation.facts());
    }

    fn assert_polyglot_languages(facts: &CanonicalGenerationFacts) {
        let languages = facts
            .files()
            .iter()
            .map(|file| file.language.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            languages,
            BTreeSet::from(["go", "javascript", "python", "rust", "typescript"])
        );
    }

    fn assert_polyglot_resolved_references(facts: &ModuleResolverFacts<'_>) {
        for expected in POLYGLOT_RESOLVED_REFERENCES {
            let caller = facts.symbol(expected.caller_path, expected.caller_name);
            let target = facts.symbol(expected.target_path, expected.target_name);
            let reference = facts.reference(&caller, expected.reference_name);
            let label = format!("{}:{}", expected.caller_path, expected.reference_name);
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                Some(&target),
                "{label}: {}",
                reference.resolution_provenance,
            );
            assert_eq!(
                reference.resolution_provenance, expected.provenance,
                "{label}"
            );
        }
    }

    fn assert_polyglot_export_edges(facts: &ModuleResolverFacts<'_>) {
        assert_source_reexport_edge(facts);
        assert_local_export_edge(facts);
        assert_commonjs_declaration_edge(facts);
        assert_rust_import_declaration_edge(facts);
    }

    fn assert_source_reexport_edge(facts: &ModuleResolverFacts<'_>) {
        let renamed = facts.symbol("barrel.ts", "renamed");
        let core = facts.symbol("core.ts", "core");
        let reference = facts.reference(&renamed, "core");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&core));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }

    fn assert_local_export_edge(facts: &ModuleResolverFacts<'_>) {
        let public_internal = facts.symbol("local_alias.ts", "publicInternal");
        let internal = facts.symbol("local_alias.ts", "internal");
        let reference = facts.reference(&public_internal, "internal");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&internal));
        assert_eq!(reference.resolution_provenance, EXACT_SAME_FILE_PROVENANCE);
    }

    fn assert_commonjs_declaration_edge(facts: &ModuleResolverFacts<'_>) {
        let target = facts.symbol("cjs_helper.js", "cjsHelper");
        let reference = facts.import_declaration_reference("cjs_destructured.cjs", "cjsHelper");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }

    fn assert_rust_import_declaration_edge(facts: &ModuleResolverFacts<'_>) {
        let target = facts.symbol("rust_helper.rs", "RUST_LIMIT");
        let reference = facts.import_declaration_reference("grouped_use.rs", "RUST_LIMIT");
        assert_eq!(reference.target_symbol_id.as_ref(), Some(&target));
        assert_eq!(reference.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }

    fn assert_polyglot_rejections(facts: &ModuleResolverFacts<'_>) {
        let bare_go_call = facts.symbol("go_bare.go", "BareGoCall");
        for name in ["Run", "ForeignOnly", "ExternalOnly", "fixture"] {
            let reference = facts.reference(&bare_go_call, name);
            assert!(reference.target_symbol_id.is_none(), "{name}");
            assert_eq!(
                reference.resolution_provenance, UNRESOLVED_PROVENANCE,
                "{name}"
            );
        }
        let cross_language = facts.symbol("cross_language.ts", "crossLanguage");
        let reference = facts.reference(&cross_language, "python_helper");
        assert!(reference.target_symbol_id.is_none());
        assert_eq!(
            reference.resolution_provenance,
            UNRESOLVED_IMPORT_PROVENANCE
        );
        let external_boundaries = facts.symbol("external_boundary.ts", "externalBoundaries");
        for (name, provenance) in [
            ("remote", EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE),
            ("value.map", DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE),
            ("Date.now", JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE),
            ("Parameters", JAVASCRIPT_INTRINSIC_UNRESOLVED_PROVENANCE),
        ] {
            let reference = facts.reference(&external_boundaries, name);
            assert!(reference.target_symbol_id.is_none(), "{name}");
            assert_eq!(reference.resolution_provenance, provenance, "{name}");
        }
        let rust_rejected = facts.symbol("lib.rs", "rust_rejected");
        for name in [
            "rust_helper::hidden",
            "rust_helper::nested_only",
            "rust_helper::orphan_method",
            "rust_helper::inner",
            "conflict::conflict_helper",
            "index_only::index_helper",
        ] {
            let reference = facts.reference(&rust_rejected, name);
            assert!(reference.target_symbol_id.is_none(), "{name}");
            assert_eq!(
                reference.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE,
                "{name}"
            );
        }
        let rust_dynamic_use = facts.symbol("lib.rs", "rust_dynamic_use");
        let ambiguous = facts.reference(&rust_dynamic_use, "worker.ambiguous_finish");
        assert!(ambiguous.target_symbol_id.is_none());
        assert_eq!(
            ambiguous.resolution_provenance,
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE
        );
        let rust_boundaries = facts.symbol("lib.rs", "rust_expected_boundaries");
        for (name, provenance) in [
            ("Remote", RUST_EXTERNAL_UNRESOLVED_PROVENANCE),
            ("Result", RUST_INTRINSIC_UNRESOLVED_PROVENANCE),
            ("Vec::new", RUST_INTRINSIC_UNRESOLVED_PROVENANCE),
            ("Ok", RUST_INTRINSIC_UNRESOLVED_PROVENANCE),
            ("assert", RUST_MACRO_UNRESOLVED_PROVENANCE),
            ("Remote::default", RUST_EXTERNAL_UNRESOLVED_PROVENANCE),
            ("serde_json::to_value", RUST_EXTERNAL_UNRESOLVED_PROVENANCE),
        ] {
            let reference = facts.reference(&rust_boundaries, name);
            assert!(reference.target_symbol_id.is_none(), "{name}");
            assert_eq!(reference.resolution_provenance, provenance, "{name}");
        }
        let use_fake = facts.symbol("use_fake.ts", "useFake");
        let reference = facts.reference(&use_fake, "fake");
        assert!(reference.target_symbol_id.is_none());
        assert_eq!(
            reference.resolution_provenance,
            UNRESOLVED_IMPORT_PROVENANCE
        );
        let dynamic_require = facts.symbol("dynamic_require.cjs", "dynamicUse");
        let reference = facts.reference(&dynamic_require, "require");
        assert!(reference.target_symbol_id.is_none());
        assert_eq!(reference.resolution_provenance, UNRESOLVED_PROVENANCE);
    }

    fn assert_polyglot_files_are_nonempty(facts: &CanonicalGenerationFacts) {
        for (path, _) in POLYGLOT_FIXTURES {
            let file = facts
                .files()
                .iter()
                .find(|file| file.normalized_path == path)
                .unwrap_or_else(|| panic!("polyglot file was not indexed: {path}"));
            assert!(
                facts
                    .symbols()
                    .iter()
                    .any(|symbol| symbol.file_id == file.file_id),
                "{path}"
            );
        }
    }

    fn write_polyglot_project(root: &std::path::Path) {
        for directory in POLYGLOT_DIRECTORIES {
            assert!(fs::create_dir(root.join(directory)).is_ok(), "{directory}");
        }
        for (path, source) in POLYGLOT_FIXTURES {
            assert!(fs::write(root.join(path), source).is_ok(), "{path}");
        }
    }

    #[test]
    fn config_rejects_zero_capacity_and_unbounded_memory() {
        let config = config(SERIAL_WORKERS);
        assert_eq!(
            config
                .maximum_stage_reservation_bytes()
                .unwrap_or_else(|error| panic!("stage reservation failed: {error}")),
            TEST_GENERATION_BYTES * RESOLVE_WORKING_MULTIPLIER
        );
        assert_matches!(
            NativePipelineParallelism::new(
                StageCapacity::new(0, 0),
                StageCapacity::new(SERIAL_WORKERS, SERIAL_WORKERS),
            ),
            Err(error) if error == NativePipelineConfigError::invalid("read_capacity")
        );
        assert_matches!(
            NativeRetainedLimits::new(
                TEST_MANIFEST_BYTES,
                MAX_PIPELINE_RETAINED_BYTES + 1,
            ),
            Err(error) if error == NativePipelineConfigError::invalid("max_generation_bytes")
        );
    }
}
