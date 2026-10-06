//! Native source discovery and extraction for Cartograph v2.

mod bounded_name;
mod budget;
mod code_scan;
mod custom;
mod discovery;
mod discovery_policy;
mod framework;
mod framework_bridge;
mod framework_bun;
mod framework_codeigniter;
mod framework_drupal;
mod framework_hono;
mod framework_managed_routes;
mod framework_manifest;
mod framework_mybatis;
mod framework_nest;
mod framework_rails;
mod framework_spring;
mod framework_symfony;
mod grammars;
mod identity;
mod language;
mod model;
mod module_alias;
mod native;
mod objc_lex;
mod objc_macro_rewrite;
mod reader;
mod snapshot;
mod source_lines;
mod tags;
mod test_names;
mod walk;

pub use budget::{native_extraction_reservation, native_output_limit, native_read_reservation};
pub use cartograph_domain::{SymbolExecutionFlags, SymbolExportFlags, SymbolImplementationFlags};
pub use discovery::{
    DiscoveredSource, DiscoveryLimits, DiscoveryLimitsError, SourceDiscoveryError,
    SourceDiscoveryOptions, SourceExclusionEvidence,
};
pub use discovery_policy::{
    DiscoveryPolicy, DiscoveryPolicyError, NestedRepositoryPolicy, V1_DEFAULT_EXCLUDES,
};
pub use grammars::NativeGrammar;
pub use language::{ExtractionStrategy, LanguageSpec};
pub use model::{
    CloneTokenCount, CloneTokenProfile, Containment, DYNAMIC_DISPATCH_RESOLUTION_PREFIX,
    DiagnosticCode, EMBEDDED_SQL_RESOLUTION_PREFIX, ExtractedFile, ExtractedImportBinding,
    ExtractedNumericalSite, ExtractedReference, ExtractedSymbol, ExtractionDiagnostic,
    ImportBindingKind, LEXICAL_SCOPE_RESOLUTION_PREFIX, PHP_EXACT_RESOLUTION_PREFIX,
    PHP_NAMESPACE_SCOPE_MODULE, PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX,
    RUST_MACRO_RESOLUTION_PREFIX, RUST_SELF_RECEIVER_RESOLUTION_PREFIX,
    SALESFORCE_COMPONENT_MODULE, SALESFORCE_CONTROLLER_MODULE, SymbolHealthMetrics,
    TYPE_QUERY_VALUE_RESOLUTION_PREFIX,
};
pub use module_alias::substitute_module_alias;
pub use native::{
    DEFAULT_MAXIMUM_AST_DEPTH, ExtractError, MAXIMUM_AST_DEPTH, MINIMUM_AST_DEPTH, NativeExtractor,
};
pub use reader::{SourceReadError, SourceReadOptions, SourceRoot};
pub use snapshot::{
    SnapshotError, SourceLimits, SourceLimitsError, SourceSnapshot, is_test_source_path,
};

/// Digest of every Rust extractor/domain source input and the locked parser dependency graph.
///
/// PostgreSQL parse-cache rows use this identity so any extraction implementation, serialized
/// fact contract, language registry, or locked grammar change becomes an automatic cache miss.
#[must_use]
pub fn native_extractor_contract_digest() -> cartograph_domain::ContentDigest {
    cartograph_domain::ContentDigest::from_bytes(
        *blake3::hash(env!("CARTOGRAPH_NATIVE_EXTRACTOR_CONTRACT").as_bytes()).as_bytes(),
    )
}
