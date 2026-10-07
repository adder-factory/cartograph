//! Retain exact AST evidence of plain root impls, opaque macro arguments and
//! pattern bindings the symbol walk cannot represent. Uncertainty fences scopes.
use cartograph_domain::{SourcePosition, SourceSpan};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, walk::ExtractionBuilder};

const EXPRESSION_MACROS: [&str; 23] = [
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "debug_assert_matches",
    "println",
    "print",
    "eprintln",
    "eprint",
    "write",
    "writeln",
    "format",
    "format_args",
    "panic",
    "unreachable",
    "todo",
    "unimplemented",
    "vec",
    "matches",
    "dbg",
    "compile_error",
];

pub(super) fn unrepresented_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    super::rust_module_scopes::file_module(builder, node)?;
    super::rust_module_scopes::module_glob(builder, node)?;
    if node.kind() == "macro_definition" {
        return standard_macro_definition(builder, node);
    }
    if node.kind() == "function_item" {
        return lifetime_function(builder, node);
    }
    if matches!(node.kind(), "mod_item" | "extern_crate_declaration") {
        return macro_import(builder, node);
    }
    if node.kind() == "macro_invocation" {
        return opaque_macro(builder, node);
    }
    if super::rust_pattern_guards::retain(builder, node)? {
        return Ok(());
    }
    let Some(name) = local_name(builder, node)? else {
        return Ok(());
    };
    unrepresented_binding(builder, (name, node))
}

pub(super) fn unrepresented_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    (mut name, node): (String, Node<'_>),
) -> Result<(), ExtractError> {
    // These are private shadow keys, not public symbol identities. Rust raw
    // identifiers share the ordinary spelling; Unicode needs NFC evidence.
    if name.starts_with("r#") {
        name.drain(..2);
    }
    if !name.is_ascii() {
        name.clear();
        name.push('*');
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-unrepresented-locals>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: name,
        span: crate::walk::syntax::span_for(node)?,
    })
}

fn standard_macro_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name)?;
    if !EXPRESSION_MACROS.contains(&name.as_str()) {
        return Ok(());
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-standard-macro-definition>".to_owned(),
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
    let item = statement_scope(node)
        .is_some_and(|scope| matches!(scope.kind(), "source_file" | "declaration_list"));
    let standard = if item {
        builtin_compile_error(builder, node)?
    } else {
        None
    };
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: if item {
            "<rust-opaque-root-macro>"
        } else {
            "<rust-opaque-macro>"
        }
        .to_owned(),
        imported_name: node.start_byte().to_string(),
        local_name: standard.unwrap_or_else(|| "*".to_owned()),
        span: crate::walk::syntax::span_for(tokens)?,
    })?;
    block_macro(builder, node)
}

fn builtin_compile_error(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(name) = node.child_by_field_name("macro") else {
        return Ok(None);
    };
    let path = super::qualified_path::lookup_name(builder, name)?;
    if !matches!(
        path.as_deref(),
        Some("compile_error" | "std::compile_error" | "core::compile_error")
    ) {
        return Ok(None);
    }
    standard_expression_macro(builder, node)
}

fn statement_scope(node: Node<'_>) -> Option<Node<'_>> {
    let parent = node.parent()?;
    if parent.kind() == "expression_statement" {
        parent.parent()
    } else {
        Some(parent)
    }
}

fn block_macro(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(block) = statement_scope(node).filter(|scope| scope.kind() == "block") else {
        return Ok(());
    };
    let standard = standard_expression_macro(builder, node)?;
    let invocation = crate::walk::syntax::span_for(node)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-opaque-block-macro>".to_owned(),
        // The invocation decides textual macro imports; its block span
        // remains the extent fenced when that identity is unproven.
        imported_name: invocation.start_byte().to_string(),
        // Complete import facts decide whether this prospective standard
        // spelling is overridden, including by a later `use` item.
        local_name: standard.unwrap_or_else(|| "*".to_owned()),
        span: crate::walk::syntax::span_for(block)?,
    })
}

fn standard_expression_macro(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(name) = node.child_by_field_name("macro") else {
        return Ok(None);
    };
    let Some(mut path) = super::qualified_path::lookup_name(builder, name)? else {
        return Ok(None);
    };
    if let Some((root, member)) = path.split_once("::")
        && matches!(root, "std" | "core")
        && EXPRESSION_MACROS.contains(&member)
    {
        let length = root.len();
        path.truncate(length);
        return Ok(Some(path));
    }
    // The optional fallback omits the macro registry, so it cannot prove
    // that a bare standard spelling has no local override.
    let proven = name.kind() == "identifier"
        && EXPRESSION_MACROS.contains(&path.as_str())
        && builder.optional_facts.records()
        && !builder
            .rust_macros
            .is_defined((&path, node.start_byte()), builder.context.cancelled)?;
    Ok(proven.then_some(path))
}

fn macro_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if !super::super::rust_macro::has_macro_use(builder, node) {
        return Ok(());
    }
    let Some(scope) = node
        .parent()
        .filter(|scope| node.end_byte() < scope.end_byte())
    else {
        return Ok(());
    };
    let span = SourceSpan::new(end_position(node)?, end_position(scope)?)
        .map_err(|_| ExtractError::InvalidSpan)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-opaque-macro-import>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span,
    })
}

fn end_position(node: Node<'_>) -> Result<SourcePosition, ExtractError> {
    let span = crate::walk::syntax::span_for(node)?;
    SourcePosition::new(span.end_byte(), span.end_line(), span.end_column())
        .map_err(|_| ExtractError::InvalidSpan)
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
        || !lifetime_parameters_only(builder, node)?
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

fn lifetime_parameters_only(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(parameters) = node.child_by_field_name("type_parameters") else {
        return Ok(true);
    };
    for parameter in super::named_children(parameters) {
        builder.context.ensure_active()?;
        if parameter.kind() != "lifetime_parameter" {
            return Ok(false);
        }
    }
    Ok(true)
}

fn lifetime_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.has_error()
        || node.child_by_field_name("type_parameters").is_none()
        || node
            .parent()
            .is_none_or(|parent| parent.kind() != "source_file")
        || !lifetime_parameters_only(builder, node)?
    {
        return Ok(());
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-lifetime-root-function>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: crate::walk::syntax::span_for(node)?,
    })
}
