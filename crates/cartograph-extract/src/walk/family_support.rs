//! Shared emission helpers for the dedicated Lisp, Lean, `ReScript`, and
//! Solidity families.
//!
//! These families describe declarations as a kind, a name node, an optional
//! body, and a literal-free signature. Centralizing the emission keeps their
//! symbols, scopes, and references uniform.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, Visibility};
use tree_sitter::Node;

use crate::{ExtractError, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, references::push_reference,
    sql_family::OwnerScopeInput, with_root_scope,
};

pub(super) use super::{ada_family::emit_import_reference, sql_family::with_owner};

/// Largest declaration name, reference name, or signature these families retain.
pub(super) const MAX_RETAINED_TEXT_BYTES: usize = 512;

/// Classification and flags of one emitted declaration.
#[derive(Clone, Copy)]
pub(super) struct DeclarationShape {
    pub(super) kind: SymbolKind,
    pub(super) exported: bool,
    pub(super) visibility: Option<Visibility>,
    pub(super) async_symbol: bool,
    /// A callable or type declared without an implementation body.
    pub(super) declaration_only: bool,
}

impl DeclarationShape {
    /// A declaration with no visibility modifier and no async marker.
    pub(super) const fn plain(kind: SymbolKind, exported: bool) -> Self {
        Self {
            kind,
            exported,
            visibility: None,
            async_symbol: false,
            declaration_only: false,
        }
    }
}

/// One declaration ready to be emitted.
pub(super) struct SymbolEmission<'tree> {
    pub(super) node: Node<'tree>,
    pub(super) name: String,
    pub(super) body: Option<Node<'tree>>,
    pub(super) signature: Option<String>,
    pub(super) shape: DeclarationShape,
}

/// Emit one declaration whose span, structure, and doc anchor are `node`.
pub(super) fn emit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: SymbolEmission<'_>,
) -> Result<SymbolId, ExtractError> {
    builder.emit_symbol(PendingSymbol {
        kind: input.shape.kind,
        name: input.name,
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: input.body,
        declaration_only: input.shape.declaration_only,
        signature: input.signature,
        export: SymbolExportFlags::named(input.shape.exported),
        async_symbol: input.shape.async_symbol,
        static_member: false,
        visibility: input.shape.visibility,
    })
}

/// Emit a root-scope import declaration that carries no module reference.
pub(super) fn emit_import_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: String,
) -> Result<SymbolId, ExtractError> {
    with_root_scope(builder, |builder| {
        emit_declaration(
            builder,
            SymbolEmission {
                node,
                name,
                body: None,
                signature: None,
                shape: DeclarationShape::plain(SymbolKind::Import, false),
            },
        )
    })
}

/// Record a per-file scope fact under `key`, reserving fallibly and within the
/// string budget like the other scope-map users.
pub(super) fn register_scope_key(
    builder: &mut ExtractionBuilder<'_, '_>,
    key: &str,
    value: Option<(SymbolId, SymbolKind)>,
) -> Result<(), ExtractError> {
    if builder.native_scope_symbols.contains_key(key) {
        return Ok(());
    }
    builder
        .native_scope_symbols
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    let key = builder.context.copy_text(key)?;
    builder.native_scope_symbols.insert(key, value);
    Ok(())
}

/// Source text of `node`, borrowed from the snapshot rather than the builder.
pub(super) fn source_text<'source>(source: &'source str, node: Node<'_>) -> &'source str {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
}

/// A source name, including escaped identifiers, screened before it is retained.
pub(super) fn screened_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let value = builder.context.text(node);
    if super::specifier_safety::specifier_may_carry_credential(value) {
        return Ok(None);
    }
    builder.context.owned_text(node).map(Some)
}

/// A declaration together with the children visited inside its scope.
pub(super) struct ScopedEmission<'tree, 'children> {
    pub(super) symbol: SymbolEmission<'tree>,
    pub(super) children: &'children [Node<'tree>],
    pub(super) depth: usize,
}

/// Emit a declaration and visit `children` with it as the innermost owner.
pub(super) fn emit_scoped_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ScopedEmission<'_, '_>,
) -> Result<SymbolId, ExtractError> {
    let kind = input.symbol.shape.kind;
    let name = builder.context.copy_text(&input.symbol.name)?;
    let id = emit_declaration(builder, input.symbol)?;
    visit_in_scope(
        builder,
        ScopeVisit {
            owner: &id,
            kind,
            name: &name,
            children: input.children,
            depth: input.depth,
        },
    )?;
    Ok(id)
}

/// Children visited inside an already emitted owner.
#[derive(Clone, Copy)]
pub(super) struct ScopeVisit<'scope, 'tree> {
    pub(super) owner: &'scope SymbolId,
    pub(super) kind: SymbolKind,
    pub(super) name: &'scope str,
    pub(super) children: &'scope [Node<'tree>],
    pub(super) depth: usize,
}

/// Visit `children` with `owner` as the innermost owner and qualifier.
pub(super) fn visit_in_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ScopeVisit<'_, '_>,
) -> Result<(), ExtractError> {
    let child_depth = input.depth.saturating_add(1);
    with_owner(
        builder,
        OwnerScopeInput {
            owner: input.owner,
            kind: input.kind,
            name: input.name,
        },
        |builder| {
            for child in input.children {
                builder.visit(*child, child_depth)?;
            }
            Ok(())
        },
    )
}

/// One reference owned by the innermost current owner.
#[derive(Clone, Copy)]
pub(super) struct OwnedReference<'tree, 'name> {
    pub(super) name: &'name str,
    pub(super) kind: ReferenceKind,
    pub(super) node: Node<'tree>,
}

/// Emit a reference owned by the innermost current owner.
pub(super) fn emit_owned_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: OwnedReference<'_, '_>,
) -> Result<(), ExtractError> {
    let owner = builder.owners.last().cloned();
    emit_reference_owned_by(builder, owner, input)
}

/// Emit a reference owned by `owner`.
pub(super) fn emit_reference_owned_by(
    builder: &mut ExtractionBuilder<'_, '_>,
    owner: Option<SymbolId>,
    input: OwnedReference<'_, '_>,
) -> Result<(), ExtractError> {
    let name = builder.context.copy_text(input.name)?;
    push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: input.kind,
            node: input.node,
        },
    )
}

/// Return `text` as a retained name when it is a bounded single token.
///
/// Names never contain whitespace, control characters, or quote characters,
/// so a malformed or literal-bearing node cannot become a symbol or reference.
pub(super) fn bounded_name(text: &str) -> Option<&str> {
    let text = text.trim();
    let valid = !text.is_empty()
        && text.len() <= MAX_RETAINED_TEXT_BYTES
        && !super::specifier_safety::specifier_may_carry_credential(text)
        && !text.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '"' | '\'' | '`')
        });
    valid.then_some(text)
}

/// Copy `text` as a signature only when it is bounded and literal-free.
pub(super) fn literal_free_signature(
    builder: &ExtractionBuilder<'_, '_>,
    text: &str,
) -> Result<Option<String>, ExtractError> {
    let text = text.trim();
    if text.is_empty()
        || text.len() > MAX_RETAINED_TEXT_BYTES
        || !cartograph_domain::callable_signature_is_literal_free(text)
    {
        return Ok(None);
    }
    builder.context.copy_text(text).map(Some)
}
