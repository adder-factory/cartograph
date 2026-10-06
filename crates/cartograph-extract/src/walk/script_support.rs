//! Shared emission helpers for the dynamic scripting families (Ruby, Lua, R, Nix).
//!
//! These languages load other files or packages through ordinary calls
//! (`require`, `source`, `library`, `import`) that bind no imported identifier.
//! A load is retained as an `Import` symbol, an owner-less `Imports` reference,
//! and an exact binding whose local name is [`LOAD_BINDING_LOCAL_NAME`] unless
//! the language makes the load addressable as a namespace (R `pkg::name`). A
//! wildcard (or any identifier-shaped) local name would let the binding claim
//! ordinary references and suppress their project resolution, which a load that
//! introduces no name does not justify.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, callable_signature_is_literal_free};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ExtractedReference, ImportBindingKind};

use super::{
    ExtractionBuilder, MAX_SAFE_SIGNATURE_BYTES, PendingSymbol,
    specifier_safety::specifier_may_carry_credential,
    syntax::{descendants_including_root, span_for},
};

/// Longest source-visible name or module specifier a scripting family retains.
pub(super) const MAX_SCRIPT_NAME_BYTES: usize = 512;
/// Local name of a load binding: not an identifier in any scripting family, so
/// no source reference can be matched to it.
pub(super) const LOAD_BINDING_LOCAL_NAME: &str = "<load>";

/// Walk state of the scripting families, held in one builder slot so their
/// source-order bookkeeping stays out of the shared builder fields.
#[derive(Default)]
pub(super) struct ScriptState {
    /// Ruby local scopes, singleton context, and emitted-method index.
    pub(super) ruby: super::ruby_family::RubyState,
    /// R function values (named or anonymous) enclosing the walked node.
    pub(super) r_function_depth: usize,
}

/// One owner frame entered while a declaration's body is visited.
#[derive(Clone, Copy)]
pub(super) struct OwnerScope<'name> {
    pub(super) id: &'name SymbolId,
    pub(super) kind: SymbolKind,
    pub(super) name: &'name str,
}

/// Run `action` with `scope` as the innermost owner, qualifier, and visibility frame.
pub(super) fn with_owner<Output>(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: OwnerScope<'_>,
    action: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<Output, ExtractError>,
) -> Result<Output, ExtractError> {
    let qualifier = builder.context.copy_text(scope.name)?;
    builder.owners.push(scope.id.clone());
    builder.native_owner_kinds.push(scope.kind);
    builder.native_visibilities.push(None);
    builder.qualifiers.push(qualifier);
    let result = action(builder);
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

/// A declaration with every optional symbol attribute at its neutral default.
pub(super) fn plain_symbol(kind: SymbolKind, name: String, node: Node<'_>) -> PendingSymbol<'_> {
    PendingSymbol {
        kind,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::named(false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    }
}

/// One file or package load that binds no source identifier of its own.
pub(super) struct LoadImport<'tree> {
    /// Syntax node whose span the symbol, reference, and binding share.
    pub(super) site: Node<'tree>,
    /// Source-visible module name retained on the `Import` symbol.
    pub(super) display: String,
    /// Exact specifier used by module resolution.
    pub(super) specifier: String,
    /// Namespace qualifier the load makes addressable (R `pkg::name`), if any.
    pub(super) namespace: Option<String>,
    /// Module-resolution semantics of the load.
    pub(super) kind: ImportBindingKind,
}

/// Emit the import symbol, owner-less import reference, and exact binding of one load.
pub(super) fn emit_load_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    load: LoadImport<'_>,
) -> Result<(), ExtractError> {
    if specifier_may_carry_credential(&load.display)
        || specifier_may_carry_credential(&load.specifier)
    {
        return Ok(());
    }
    let span = span_for(load.site)?;
    let site = load.site;
    let display = load.display;
    super::with_root_scope(builder, |builder| {
        builder.emit_symbol(plain_symbol(SymbolKind::Import, display, site))
    })?;
    builder.emit_reference(ExtractedReference {
        owner: None,
        name: builder.context.copy_text(&load.specifier)?,
        resolution_name: None,
        kind: ReferenceKind::Imports,
        span,
    })?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: load.kind,
        module_specifier: load.specifier,
        imported_name: "*".to_owned(),
        local_name: load
            .namespace
            .unwrap_or_else(|| LOAD_BINDING_LOCAL_NAME.to_owned()),
        span,
    })
}

/// Trimmed node text when it is a bounded single-line name without quotes.
pub(super) fn bounded_name(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    bounded_text(builder, builder.context.text(node))
}

/// `value` as an owned name when it is bounded, single-line, and quote-free.
pub(super) fn bounded_text(
    builder: &ExtractionBuilder<'_, '_>,
    value: &str,
) -> Result<Option<String>, ExtractError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_SCRIPT_NAME_BYTES
        || specifier_may_carry_credential(value)
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | '\'' | '`'))
    {
        return Ok(None);
    }
    builder.context.copy_text(value).map(Some)
}

/// `value` as a reference name: bounded, quote-free, and free of whitespace,
/// grouping, and indexing syntax, so no argument or computed receiver text
/// (which may carry literals) can become part of a retained name.
pub(super) fn bounded_reference_name(
    builder: &ExtractionBuilder<'_, '_>,
    value: &str,
) -> Result<Option<String>, ExtractError> {
    if value.chars().any(|character| {
        character.is_whitespace() || matches!(character, '(' | ')' | '[' | ']' | '{' | '}')
    }) {
        return Ok(None);
    }
    bounded_text(builder, value)
}

/// `signature` with whitespace runs collapsed, when it is bounded and literal-free.
pub(super) fn literal_free_signature(
    builder: &ExtractionBuilder<'_, '_>,
    signature: &str,
) -> Result<Option<String>, ExtractError> {
    let signature = signature.trim();
    if signature.is_empty()
        || signature.len() > MAX_SAFE_SIGNATURE_BYTES
        || !callable_signature_is_literal_free(signature)
    {
        return Ok(None);
    }
    builder
        .context
        .budget
        .ensure_string_length(signature.len())?;
    let mut collapsed = String::new();
    collapsed
        .try_reserve(signature.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    for word in signature.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }
    Ok(Some(collapsed))
}

/// `signature` for the syntax rooted at `source`, withheld when that syntax
/// contains any literal node (defaults such as `:sym`, `%q(...)`, `./path`, or
/// string singleton types) that the textual guard cannot recognize.
pub(super) fn literal_free_node_signature(
    builder: &ExtractionBuilder<'_, '_>,
    source: Node<'_>,
    signature: &str,
) -> Result<Option<String>, ExtractError> {
    if contains_literal(source) {
        return Ok(None);
    }
    literal_free_signature(builder, signature)
}

/// Whether the syntax under `node` contains a literal-kinded node; syntax
/// longer than a retained signature counts as literal-bearing.
pub(super) fn contains_literal(node: Node<'_>) -> bool {
    node.end_byte().saturating_sub(node.start_byte()) > MAX_SAFE_SIGNATURE_BYTES
        || descendants_including_root(node)
            .any(|child| child.is_named() && is_literal_kind(child.kind()))
}

/// `= <value>` for an assignment whose value contains no literal syntax node.
///
/// The shared textual guard cannot recognize every scripting literal form
/// (Lua `[[long strings]]`, Ruby `%w[...]`/`%q(...)`, symbols, heredoc
/// openers), so any literal-kinded node in the value withholds the signature.
pub(super) fn literal_free_assignment_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    value: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if contains_literal(value) {
        return Ok(None);
    }
    super::safe_assignment_signature(builder, value)
}

fn is_literal_kind(kind: &str) -> bool {
    LITERAL_KIND_FRAGMENTS
        .iter()
        .any(|fragment| kind.contains(fragment))
}

/// Node-kind fragments that mark literal syntax across the scripting grammars.
const LITERAL_KIND_FRAGMENTS: [&str; 12] = [
    "string",
    "heredoc",
    "symbol",
    "character",
    "integer",
    "float",
    "number",
    "rational",
    "complex",
    "regex",
    "literal",
    "path_expression",
];

/// Whether `specifier` names a path relative to the importing file.
pub(super) fn is_relative_specifier(specifier: &str) -> bool {
    matches!(specifier, "." | "..") || specifier.starts_with("./") || specifier.starts_with("../")
}
