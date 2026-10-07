//! Exact conventional file-module declarations inside inline module blocks.
use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, walk::ExtractionBuilder};
use tree_sitter::Node;

pub(super) fn file_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "mod_item"
        || node.has_error()
        || node.child_by_field_name("body").is_some()
        || !conventional_attributes(builder, node, false)?
        || !inline_scope(builder, node)?
    {
        return Ok(());
    }
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let imported_name = builder.context.owned_text(name)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-inline-file-module>".to_owned(),
        imported_name,
        local_name: builder.qualifiers.join("::"),
        span: crate::walk::syntax::span_for(node)?,
    })
}

pub(super) fn module_glob(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "use_declaration" || !inline_scope(builder, node)? {
        return Ok(());
    }
    let Some(argument) = node.child_by_field_name("argument") else {
        return Ok(());
    };
    let mut path = builder.context.owned_text(argument)?;
    if !path.contains('*') {
        return Ok(());
    }
    if node.has_error() || !conventional_attributes(builder, node, false)? {
        "*".clone_into(&mut path);
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-scoped-module-glob>".to_owned(),
        imported_name: path,
        local_name: "*".to_owned(),
        span: crate::walk::syntax::span_for(node)?,
    })
}

fn inline_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let mut parent = node.parent();
    let mut inline = false;
    while let Some(scope) = parent {
        builder.context.ensure_active()?;
        match scope.kind() {
            "source_file" => return Ok(inline),
            "declaration_list" => {}
            "mod_item" => {
                if scope.has_error() || !conventional_attributes(builder, scope, true)? {
                    return Ok(false);
                }
                inline = true;
            }
            _ => return Ok(false),
        }
        parent = scope.parent();
    }
    Ok(false)
}

fn conventional_attributes(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    allow_cfg: bool,
) -> Result<bool, ExtractError> {
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        builder.context.ensure_active()?;
        if matches!(sibling.kind(), "line_comment" | "block_comment") {
            previous = sibling.prev_named_sibling();
            continue;
        }
        if sibling.kind() != "attribute_item" {
            return Ok(true);
        }
        let Some(path) = sibling
            .named_child(0)
            .and_then(|attribute| attribute.named_child(0))
            .filter(|path| path.kind() == "identifier")
        else {
            return Ok(false);
        };
        if !allow_cfg || builder.context.text(path) != "cfg" {
            return Ok(false);
        }
        previous = sibling.prev_named_sibling();
    }
    Ok(true)
}
