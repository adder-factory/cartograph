use cartograph_domain::{
    ContentDigest, FileId, FileParseStatus, NormalizedPath, NumericalSiteId, ReferenceKind,
    SourceLanguage, SourceSpan, SymbolExecutionFlags, SymbolExportFlags, SymbolId,
    SymbolImplementationFlags, SymbolKind, Visibility,
};
use serde::{Deserialize, Serialize};

/// Internal lookup marker for bounded same-file dynamic-dispatch references.
///
/// The persisted reference keeps the source-visible target name; the indexer removes this marker
/// before lookup and records lower-confidence dispatch provenance on the resolved edge.
#[doc(hidden)]
pub const DYNAMIC_DISPATCH_RESOLUTION_PREFIX: &str = "cartograph.dynamic-dispatch::";

/// A Python use whose file-wide bindings block import fallback, preserving lexical lookup.
pub const PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX: &str = "cartograph.python-unbound-import::";

/// Internal marker for a Rust macro invocation whose declaration would require expansion.
///
/// The persisted reference keeps the source-visible macro name. The indexer removes this marker
/// before lookup and records explicit unexpanded-macro provenance instead of treating expected
/// macro-expansion uncertainty as a missing project call target.
#[doc(hidden)]
pub const RUST_MACRO_RESOLUTION_PREFIX: &str = "cartograph.rust-macro::";

/// Internal lookup marker for a Rust `self` call with syntax-proven receiver ownership.
///
/// The suffix is an enclosing nominal implementation type and member, or only the member
/// when the syntax cannot prove a nominal type. Neither form permits global name guessing.
/// Pascal reuses the same contract for implicit-`Self`, nested-routine, unit-routine, and
/// in-file type-qualified calls: `Type::Member` (or `self::Routine` for a top-level routine)
/// resolves only by exact same-file qualified name.
#[doc(hidden)]
pub const RUST_SELF_RECEIVER_RESOLUTION_PREFIX: &str = "cartograph.rust-self::";

/// Internal lookup marker for a TypeScript `typeof` query against a runtime value declaration.
///
/// The persisted reference remains a typed `type_of` edge. The indexer removes this marker and
/// permits value-declaration resolution only for the marked source shape.
#[doc(hidden)]
pub const TYPE_QUERY_VALUE_RESOLUTION_PREFIX: &str = "cartograph.type-query-value::";

/// Internal lookup marker for a JavaScript-family bare value read the extractor bound to a
/// nested lexical declaration whose qualified name another symbol of the file shares.
///
/// The suffix is the source-visible name. The indexer resolves it only by walking the
/// reference owner's enclosing scopes outward, never by exact same-file qualified name (which
/// could bind the namesake, or an embedded component script's same-named file component) and
/// never through imports or project names.
#[doc(hidden)]
pub const LEXICAL_SCOPE_RESOLUTION_PREFIX: &str = "cartograph.lexical-scope::";

/// Internal receiver-evidence lookup for an explicit same-file type.
/// The suffix is `import::type#member`, or `@symbol-id#member` for a local nominal type.
/// `?#member` fences a closer binding without usable type evidence. This is
/// syntax evidence only: project class identity and ancestry belong to resolution.
/// An empty member records the declaration binding of a Python base reference.
#[doc(hidden)]
pub const EXPLICIT_RECEIVER_RESOLUTION_PREFIX: &str = "cartograph.explicit-receiver::";

/// A receiver type bound by an import at its declaration, independent of locals at its use.
#[doc(hidden)]
pub const EXPLICIT_RECEIVER_IMPORT_PREFIX: &str = "import::";

/// Internal lookup marker for a static SQL-literal table reference.
///
/// The suffix is `<operation>::<qualified-table>`. The persisted reference keeps only the
/// source-visible table name; the indexer uses the marker to require a SQL table target and to
/// retain read/write/DDL provenance on the resulting graph edge.
#[doc(hidden)]
pub const EMBEDDED_SQL_RESOLUTION_PREFIX: &str = "cartograph.embedded-sql::";

/// Internal lookup marker for a PHP name the extractor resolved at compile time.
///
/// The suffix is `<intent>::<key>`: the intent (`class`, `function`, `function-fallback`,
/// `member`, or `constant`) names the PHP symbol space (`adapted-class` is `class` for a trait
/// used with an `as`/`insteadof` adaptation block), and the key is the exact candidate
/// qualified name (`App\Models::User::find`). The persisted reference keeps the
/// source-visible name; the indexer resolves the key by exact qualified name only and never
/// falls back to short-name matching. `function-fallback` additionally tries the global
/// function when no namespaced function of that name exists, as PHP does at runtime.
/// `dispatch-class` and `dispatch-member` name the declaration in the statically known class
/// of `static::` or `$this->`, which runtime dispatch can replace; `returned-member` keys
/// `Class::factory::member` name a factory method and the member called on the object it
/// returns, followed only through the factory's declared return type (`own-returned-member`
/// when the call is inside the factory's own class, so a non-public factory is accessible);
/// and `abstain` marks a
/// static name whose target cannot be determined exactly. Class, function, and method names
/// match ignoring ASCII case, as in PHP; constant names match exactly.
#[doc(hidden)]
pub const PHP_EXACT_RESOLUTION_PREFIX: &str = "cartograph.php-exact::";

/// Internal module identity of an implicit Salesforce controller binding.
/// Its import binding retains the exact controller attribute site and name.
#[doc(hidden)]
pub const SALESFORCE_CONTROLLER_MODULE: &str = "cartograph.salesforce-controller";

/// Internal module identity of an implicit Salesforce component binding.
/// Its imported name preserves bundle case independently of the reference name.
#[doc(hidden)]
pub const SALESFORCE_COMPONENT_MODULE: &str = "cartograph.salesforce-component";

/// Internal namespace-block marker, including PHP's unnamed global blocks.
#[doc(hidden)]
pub const PHP_NAMESPACE_SCOPE_MODULE: &str = "cartograph.php-namespace-scope";

/// Complete storage-independent output for one native source-file extraction.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedFile {
    /// Stable file identity derived from the canonical path.
    pub file_id: FileId,
    /// Canonical project-relative path.
    pub path: NormalizedPath,
    /// Native grammar used for this file.
    pub language: SourceLanguage,
    /// Digest of the exact original UTF-8 bytes.
    pub content_hash: ContentDigest,
    /// Original source size before parsing.
    pub byte_size: u64,
    /// Exact one-based source line count.
    pub line_count: u32,
    /// Complete, recoverably partial, or failed parse state.
    pub parse_status: FileParseStatus,
    /// Source-ordered declarations.
    pub symbols: Vec<ExtractedSymbol>,
    /// Source-ordered lexical containment relationships.
    pub containments: Vec<Containment>,
    /// Source-ordered unresolved structural references.
    pub references: Vec<ExtractedReference>,
    /// Syntax-proven callable scope at exact call sites, separate from reference identity.
    #[serde(default)]
    pub call_scope_sites: Vec<ExtractedCallScopeSite>,
    /// Scope or constructor evidence for JavaScript member calls, keyed by terminal token end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub javascript_member_calls: Vec<JavascriptMemberCallContext>,
    /// Syntax-proven value shadows or object declarations that forbid a
    /// nominal receiver or constructor interpretation at the recorded span.
    /// Kept separate so source-visible reference lookup identities remain stable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolution_abstentions: Vec<SourceSpan>,
    /// Java local type declarations paired with their exact enclosing block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_type_scopes: Vec<(SourceSpan, SourceSpan)>,
    /// Optional explicit receiver and member-shadowing evidence. An absent
    /// payload keeps unrelated languages' core output within its original bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_evidence: Option<Box<ExtractedReceiverEvidence>>,
    /// Source-ordered, privacy-safe static numerical evidence sites.
    pub numerical_sites: Vec<ExtractedNumericalSite>,
    /// Source-ordered ES module bindings used by project-wide resolution.
    pub import_bindings: Vec<ExtractedImportBinding>,
    /// Whether this source contains an AST-confirmed inline test declaration.
    pub has_inline_tests: bool,
    /// Bounded test-suite/case titles and test declaration names for BM25 intent lookup.
    pub test_search_text: String,
    /// Whether the per-file test-name search text hit its byte ceiling.
    pub test_search_truncated: bool,
    /// Bounded, credential-safe parse diagnostics.
    pub diagnostics: Vec<ExtractionDiagnostic>,
}

/// The bounded syntax proof that permits an enclosing-scope call lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallScopeKind {
    /// A direct method or its lexical receiver closure preserves current-class ownership.
    CurrentClass,
    /// A call stays in the retained callable's binding scope without an anonymous boundary.
    DirectCallable,
}

/// One call site whose syntax establishes retained callable or class ownership.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedCallScopeSite {
    /// Retained declaration containing the call.
    pub owner: SymbolId,
    /// Exact source range of the call reference.
    pub span: SourceSpan,
    /// Scope established by the native syntax walk.
    pub kind: CallScopeKind,
}

/// Receiver evidence that does not change a reference's source identity or lookup key.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavascriptMemberCallContext {
    /// End byte of the terminal member token shared by its qualified and dynamic call facts.
    pub end_byte: u64,
    /// Exact lexical or direct-constructor context established by the source AST.
    pub receiver: JavascriptMemberReceiver,
}

/// The receiver context relevant to bounded JavaScript member refinements.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavascriptMemberReceiver {
    /// A parameter, local, catch, or destructuring binding in an enclosing scope.
    Shadowed,
    /// An enclosing scope whose receiver binding could not be established.
    Uncertain,
    /// A local import binding whose existing import resolution must remain available.
    LocalImport,
    /// A direct member access on a named, unshadowed constructor expression.
    Constructor(String),
}

/// One exact source site where static syntax exposes numerical behavior or risk.
///
/// No source expression or literal value is retained. The structural digest and
/// bounded categorical fields let numerical tooling find the site without
/// conflating a static heuristic with a runtime observation.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedNumericalSite {
    /// Stable generation-independent identity derived from file, span, and category.
    pub id: NumericalSiteId,
    /// Closest extracted declaration containing the expression.
    pub owner: Option<SymbolId>,
    /// Exact expression source range.
    pub span: SourceSpan,
    /// Stable numerical operation category.
    pub operation: String,
    /// Stable potential-hazard category, or `none_observed`.
    pub hazard: String,
    /// Best statically visible precision category.
    pub precision: String,
    /// Source-version-fenced, privacy-safe expression identity digest.
    pub expression_digest: ContentDigest,
    /// Deterministic heuristic confidence in parts per million.
    pub confidence_ppm: u32,
    /// Extractor contract that produced this site.
    pub provenance: String,
    /// Stable comma-separated list of facts static analysis could not prove.
    pub unknowns: String,
}

/// Syntax facts that disambiguate declarations with the same symbol kind and name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclarationSyntax {
    /// No declaration-specific syntax is needed for resolution.
    #[default]
    Other,
    /// A Kotlin primary or secondary constructor, rather than a same-named function.
    KotlinConstructor,
}

/// One normalized declaration emitted by a native language extractor.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedSymbol {
    /// Stable line-independent identity.
    pub id: SymbolId,
    /// Declaration category.
    pub kind: SymbolKind,
    /// Local declaration name.
    pub name: String,
    /// Scope-qualified declaration name.
    pub qualified_name: String,
    /// Exact source range.
    pub span: SourceSpan,
    /// Deterministic declaration signature when present.
    pub signature: Option<String>,
    /// Human-authored `JSDoc` immediately preceding the declaration.
    pub docstring: Option<String>,
    /// Bounded identifier/keyword text from the implementation with literals omitted.
    pub body_search_text: String,
    /// Whether implementation search text stopped at its per-symbol byte ceiling.
    pub body_search_truncated: bool,
    /// Privacy-safe structural and agent-prone detector counters computed from the body AST/text.
    pub health: SymbolHealthMetrics,
    /// Implementation and test-ownership state.
    #[serde(flatten)]
    pub implementation: SymbolImplementationFlags,
    /// Explicit module export state.
    #[serde(flatten)]
    pub export: SymbolExportFlags,
    /// Async and static execution modifiers.
    #[serde(flatten)]
    pub execution: SymbolExecutionFlags,
    /// Syntax-proven declaration category used for resolution.
    #[serde(default)]
    pub declaration_syntax: DeclarationSyntax,
    /// Explicit declaration visibility.
    pub visibility: Option<Visibility>,
    /// Whitespace/comment-independent concrete-syntax digest.
    pub structural_digest: ContentDigest,
    /// Identifier/literal-normalized AST-shape digest for Type-2 clone detection.
    pub clone_shape_digest: ContentDigest,
    /// Bounded identifier-preserving leaf-token multiset for Type-3 clone detection.
    ///
    /// Token text is never retained: each token is represented by a domain-separated
    /// fingerprint and occurrence count. `None` means this extractor could not produce a
    /// complete profile within the per-symbol safety bound; exact and Type-2 detection remain
    /// available in that case.
    pub clone_token_profile: Option<CloneTokenProfile>,
}

/// One privacy-safe token occurrence entry in a Type-3 clone profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneTokenCount(pub u64, pub u32);

/// Complete bounded multiset used for exact SourcererCC-style overlap comparisons.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneTokenProfile {
    counts: Vec<CloneTokenCount>,
    total_tokens: u32,
    identifier_counts: Vec<CloneTokenCount>,
    identifier_tokens: u32,
}

pub(crate) struct CloneTokenProfileInput {
    pub(crate) counts: Vec<CloneTokenCount>,
    pub(crate) total_tokens: u32,
    pub(crate) identifier_counts: Vec<CloneTokenCount>,
    pub(crate) identifier_tokens: u32,
}

impl CloneTokenProfile {
    pub(crate) fn new(input: CloneTokenProfileInput) -> Self {
        let CloneTokenProfileInput {
            counts,
            total_tokens,
            identifier_counts,
            identifier_tokens,
        } = input;
        Self {
            counts,
            total_tokens,
            identifier_counts,
            identifier_tokens,
        }
    }

    /// Sorted token fingerprints and their occurrence counts.
    #[must_use]
    pub fn counts(&self) -> &[CloneTokenCount] {
        &self.counts
    }

    /// Number of tokens in the original leaf stream, including repeats.
    #[must_use]
    pub const fn total_tokens(&self) -> u32 {
        self.total_tokens
    }

    /// Sorted privacy-safe identifier fingerprints and occurrence counts.
    #[must_use]
    pub fn identifier_counts(&self) -> &[CloneTokenCount] {
        &self.identifier_counts
    }

    /// Identifier leaves retained for semantic clone compatibility.
    #[must_use]
    pub const fn identifier_tokens(&self) -> u32 {
        self.identifier_tokens
    }

    /// Retained heap bytes charged to the native extraction budget.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.counts
            .capacity()
            .saturating_add(self.identifier_counts.capacity())
            .saturating_mul(std::mem::size_of::<CloneTokenCount>())
    }
}

/// Bounded per-symbol metrics retained without source literals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolHealthMetrics {
    /// Distinct source rows containing owned syntax, with comments and opaque literals collapsed.
    pub code_lines: u32,
    /// Number of parameter entries.
    pub parameter_count: u16,
    /// Cyclomatic-complexity score for the symbol.
    pub cyclomatic: u16,
    /// Maximum control-flow nesting depth.
    pub max_nesting: u16,
    /// Maximum operands observed in one conditional expression.
    pub max_conditional_operands: u16,
    /// Bytes covered by outermost string and numeric literals inside the symbol body.
    pub literal_bytes: u32,
    /// Suspicious numeric literals not covered by a named-constant exemption.
    pub magic_numbers: u16,
    /// Hardcoded URL literals found in executable code.
    pub hardcoded_urls: u16,
    /// Actionable hardcoded URL literals passed to request/client operations.
    pub hardcoded_url_requests: u16,
    /// Actionable hardcoded URL literals assigned to endpoint-like configuration.
    pub hardcoded_url_configuration: u16,
    /// Presentation, navigation, and vendor-asset URL literals intentionally abstained.
    pub hardcoded_url_presentation_abstentions: u16,
    /// Credential/PII handling confidence on a bounded integer 0-100 scale.
    pub secrets_score: u16,
    /// Bit-set of the privacy-safe secret-signal categories contributing to the score.
    pub secrets_signal_mask: u16,
    /// Whether literal, environment, or exposure evidence makes secret handling actionable.
    pub secrets_actionable: bool,
    /// Count of documented numeric claims disjoint from a constant's current value.
    pub stale_doc_numbers: u16,
    /// Nested-iteration patterns with a likely accidental quadratic cost.
    pub accidental_quadratic: u16,
    /// Catch or rescue clauses with an empty body.
    pub empty_catches: u16,
    /// Synchronous I/O calls made from an asynchronous declaration.
    pub sync_io_in_async: u16,
    /// Loops that await each iteration sequentially.
    pub sequential_await_loops: u16,
    /// Awaited loops abstained because an awaited result feeds the next iteration.
    pub serial_await_dependency_loops: u16,
    /// Awaited loops abstained because post-await break or return preserves ordering.
    pub serial_await_control_flow_loops: u16,
    /// Awaited loops abstained through an explicit `cartograph: serial-await` marker.
    pub serial_await_intent_loops: u16,
    /// Nested delegate methods assembled into a returned facade object.
    pub facade_factory_delegates: u16,
    /// True when a function is a cohesive returned-object facade factory.
    pub facade_factory: bool,
    /// TypeScript casts that explicitly erase a value to `any`.
    pub ts_any_casts: u16,
    /// TypeScript diagnostic-suppression directives.
    pub ts_suppressions: u16,
    /// Debug logging calls retained in the declaration.
    pub debug_logs: u16,
    /// TODO-like incomplete-implementation markers.
    pub incomplete_markers: u16,
    /// Dynamic code-evaluation calls.
    pub dynamic_eval: u16,
    /// Uses of cryptographic hashes that are unsuitable for security decisions.
    pub insecure_hash: u16,
    /// Uses of non-cryptographic randomness in security-sensitive shapes.
    pub insecure_random: u16,
    /// HTTP client operations with no detected timeout.
    pub http_without_timeout: u16,
    /// SQL statements assembled through string concatenation or interpolation.
    pub sql_string_concatenation: u16,
    /// JSON parses whose result crosses a boundary without validation.
    pub unsafe_json_parse: u16,
    /// Environment values consumed without detected validation.
    pub unvalidated_env: u16,
    /// Declarations with an unexpectedly empty implementation body.
    pub empty_body: u16,
}

/// Lexical parent-child relationship between two extracted symbols.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Containment {
    /// Enclosing declaration.
    pub parent: SymbolId,
    /// Directly enclosed declaration.
    pub child: SymbolId,
}

/// One unresolved source reference with its closest extracted owner.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedReference {
    /// Closest function, method, class, interface, enum, or binding owner.
    pub owner: Option<SymbolId>,
    /// Normalized target name or module specifier.
    pub name: String,
    /// Optional framework-qualified lookup identity while retaining the source-visible name.
    pub resolution_name: Option<String>,
    /// Structural relationship requested from resolution.
    pub kind: ReferenceKind,
    /// Exact source range recorded for the reference expression.
    pub span: SourceSpan,
}

/// Bounded explicit receiver evidence consumed only by the project resolver.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedReceiverEvidence {
    /// Existing references' spans/kinds and their explicit receiver lookup.
    pub lookups: Vec<ExtractedReceiverLookup>,
    /// Non-method bindings that fence inherited member lookup.
    pub bindings: Vec<ExtractedReceiverBinding>,
}

/// One receiver lookup, separate from the reference's frozen syntax identity.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedReceiverLookup {
    /// The syntax reference whose receiver was established.
    pub span: SourceSpan,
    /// Distinguishes a member call from a field read at the same syntax site.
    pub kind: ReferenceKind,
    /// An internal lookup marker containing only type/member identifiers.
    pub lookup: String,
}

/// A class-body binding that cannot be assumed to be an inherited method.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedReceiverBinding {
    /// Exact local class owning the declaration. `None` retains an assignment
    /// whose receiver class cannot be established during extraction.
    pub class_id: Option<SymbolId>,
    /// The declared or shadowed member. `None` fences all member lookup after
    /// an unsupported class-body binding.
    pub name: Option<String>,
    /// An assignment through a receiver fences every lookup of this member.
    #[serde(default)]
    pub assigned: bool,
}

/// Source import/binding category with explicit module and visibility semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportBindingKind {
    /// `import Name from './module'`.
    Default,
    /// `import { Name as Local } from './module'`.
    Named,
    /// `import * as Local from './module'`.
    Namespace,
    /// `export * from './module'`, expanded after project-wide module resolution.
    ReExportAll,
    /// Syntax-recovered wildcard exports remain explicitly unresolved.
    ReExportUncertain,
    /// `export * as Local from './module'`, retaining the exported namespace owner.
    ReExportNamespace,
    /// Rust `pub use path::Name`, retaining its public facade path and exact source path.
    ReExportNamed,
    /// C-family `#include "project/header.h"` resolved against project files.
    IncludeQuoted,
    /// C-family `#include <system/header.h>` retained but never guessed as a project file.
    IncludeSystem,
}

/// One source-level module/include binding retained for exact project resolution.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedImportBinding {
    /// Binding/include semantics.
    pub kind: ImportBindingKind,
    /// Original bounded module specifier without quotes.
    pub module_specifier: String,
    /// Exported name, `default`, or `*` for a namespace/include binding.
    pub imported_name: String,
    /// Identifier visible in the importing file, or `*` for an include.
    pub local_name: String,
    /// Span of the imported name for named imports, otherwise the local identifier.
    pub span: SourceSpan,
}

/// Stable category for a recoverable extraction diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    /// Tree-sitter recovered through one or more syntax-error nodes.
    SyntaxError,
    /// A parser recovery node exposed an empty or otherwise invalid source
    /// range. The file remains present as partial, but unsafe facts are omitted.
    InvalidSpan,
    /// The parser stopped without cancellation before it produced a syntax
    /// tree. The file remains present as partial with no inferred facts.
    ParserStopped,
    /// A synthesized qualified or reference name exceeded its canonical byte
    /// bound and was deterministically shortened so the generation stays
    /// publishable. The shortened form keeps a `~` marker and a digest of the
    /// exact original name.
    CanonicalNameTruncated,
    /// A defensive AST nesting limit was reached. The file remains in the
    /// generation as recoverably partial while its untrusted subtree is omitted.
    NestingLimitExceeded,
    /// Optional enrichment facts (package bindings, struct fields, constant
    /// reads, literal instantiations, declared-type uses, supertraits, and
    /// decorators or bridge facts) exceeded their output or work budget and
    /// were omitted. The file's ordinary declarations and calls are complete.
    OptionalFactsOmitted,
}

/// Bounded diagnostic that never embeds source text or project paths.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionDiagnostic {
    /// Stable machine-readable category.
    pub code: DiagnosticCode,
    /// Error-node range when it was non-empty and representable.
    pub span: Option<SourceSpan>,
}
