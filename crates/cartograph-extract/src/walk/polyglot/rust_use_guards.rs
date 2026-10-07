//! Retain exact AST evidence of plain root impls, opaque macro arguments and
//! pattern bindings the symbol walk cannot represent. Uncertainty fences scopes.
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, walk::ExtractionBuilder};

pub(super) fn unrepresented_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() == "macro_invocation" {
        return opaque_macro(builder, node);
    }
    let Some(name) = local_name(builder, node)? else {
        return Ok(());
    };
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-unrepresented-locals>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: name,
        span: crate::walk::syntax::span_for(node)?,
    })
}

fn opaque_macro(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(tokens) = super::named_children(node).find(|child| child.kind() == "token_tree")
    else {
        return Ok(());
    };
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: if at_module_root(node) {
            "<rust-opaque-root-macro>"
        } else {
            "<rust-opaque-macro>"
        }
        .to_owned(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: crate::walk::syntax::span_for(tokens)?,
    })
}

fn at_module_root(node: Node<'_>) -> bool {
    let parent = node.parent().and_then(|parent| {
        if parent.kind() == "expression_statement" {
            parent.parent()
        } else {
            Some(parent)
        }
    });
    parent.is_some_and(|parent| matches!(parent.kind(), "source_file" | "declaration_list"))
}

fn local_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let pattern = match node.kind() {
        "let_declaration" | "let_condition" | "for_expression" | "match_arm" => {
            node.child_by_field_name("pattern")
        }
        "closure_expression" => {
            return Ok(node
                .child_by_field_name("parameters")
                .filter(|parameters| parameters.named_child_count() > 0)
                .map(|_| "*".to_owned()));
        }
        "parameter" => {
            let pattern = node.child_by_field_name("pattern");
            if pattern.is_some_and(|pattern| {
                super::super::unwrap_single_child(pattern, 0, super::RUST_PARAMETER_UNWRAP)
                    .is_some()
            }) {
                return Ok(None);
            }
            return Ok(Some("*".to_owned()));
        }
        _ => return Ok(None),
    };
    match pattern.map(|pattern| (pattern, pattern.kind())) {
        Some((pattern, "identifier")) => builder.context.owned_text(pattern).map(Some),
        Some((_, "_" | "wildcard_pattern")) => Ok(None),
        _ => Ok(Some("*".to_owned())),
    }
}

pub(super) fn plain_impl(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.has_error()
        || node.child_by_field_name("type_parameters").is_some()
        || node
            .parent()
            .is_none_or(|parent| parent.kind() != "source_file")
    {
        return Ok(());
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-plain-root-impl>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: crate::walk::syntax::span_for(node)?,
    })
}
