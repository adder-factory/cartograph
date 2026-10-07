//! Dart constructor redirects and extension-type representation constructors.

use super::{
    ExtractionBuilder, PendingSymbol, ReferenceKind, SymbolKind, named_child_of_kind,
    named_children, span_for,
};
use crate::{ExtractError, ExtractedReference};
use tree_sitter::Node;

const REDIRECT_PREFIX: &str = "dart-constructor-redirect:";

pub(super) fn static_context(callable: super::Callable<'_>) -> bool {
    super::has_child_kind(callable.signature, "static")
        || matches!(
            callable.inner.kind(),
            "factory_constructor_signature" | "redirecting_factory_constructor_signature"
        )
}

pub(in super::super) fn declaration_syntax(
    (node, kind): (Node<'_>, SymbolKind),
) -> crate::DeclarationSyntax {
    if kind != SymbolKind::Method {
        return crate::DeclarationSyntax::Other;
    }
    if node.kind() == "representation_declaration"
        || super::named_signature(node)
            .is_some_and(|(inner, _)| super::CONSTRUCTOR_SIGNATURE_KINDS.contains(&inner.kind()))
    {
        crate::DeclarationSyntax::DartConstructor
    } else {
        crate::DeclarationSyntax::Other
    }
}

pub(super) fn capture_redirect(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let receiver = match node.kind() {
        "initializer_list_entry" => named_child_of_kind(node, "super"),
        "redirection" => named_child_of_kind(node, "this"),
        _ => None,
    };
    let Some(receiver) = receiver else {
        return Ok(());
    };
    if named_child_of_kind(node, "arguments").is_none() {
        return Ok(());
    }
    let base = builder.context.text(receiver);
    let member = named_children(node).find(|child| child.kind() == "identifier");
    let name = match member {
        Some(member) => super::super::joined_signature(
            builder,
            super::super::JoinedSignature::dotted(base, builder.context.text(member)),
        )?,
        None => builder.context.copy_text(base)?,
    };
    let resolution_name = builder
        .context
        .copy_text(&format!("{REDIRECT_PREFIX}{name}"))?;
    builder.emit_reference(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name,
        resolution_name: Some(resolution_name),
        kind: ReferenceKind::Calls,
        span: span_for(node)?,
    })
}

pub(super) fn emit_representation(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "extension_type_declaration" {
        return Ok(());
    }
    let Some(representation) = node.child_by_field_name("representation") else {
        return Ok(());
    };
    let name_node = named_children(representation)
        .take_while(|child| child.kind() != "type_identifier")
        .find(|child| {
            child.kind() == "identifier"
                && Some(*child) != representation.child_by_field_name("name")
        });
    let Some(class_name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder
        .context
        .owned_text(name_node.unwrap_or(class_name))?;
    let signature = super::bounded_signature(
        builder,
        super::super::JoinedSignature::words("", builder.context.text(representation)),
    )?;
    builder.emit_symbol(PendingSymbol {
        signature,
        visibility: Some(super::dart_visibility(&name)),
        ..PendingSymbol::plain(SymbolKind::Method, name, representation)
    })?;
    let Some(field_name) = representation.child_by_field_name("name") else {
        return Ok(());
    };
    super::emit_leaf(
        builder,
        super::NamedDeclaration::new(representation, field_name, SymbolKind::Field),
    )?;
    Ok(())
}
