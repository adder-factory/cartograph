//! Carry static value paths and root opens separately from terminal-name tags.
use super::{
    ExtractionBudget, MAX_TAG_AST_DEPTH, MAXIMUM_TAG_AST_NODES, MINIMUM_TAG_AST_NODES,
    TAG_AST_NODES_PER_SOURCE_BYTE, TagExtractionInput, module_bindings, span_for,
};
use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};
use cartograph_domain::SourceLanguage;
use tree_sitter::Node;

const MAX_PATH_BYTES: usize = 1_024;
const CALL_METADATA: &str = "<ocaml-call>";
const OPEN_METADATA: &str = "<ocaml-open>";
const FENCE_METADATA: &str = "<ocaml-fence>";
const SHADOW_METADATA: &str = "<ocaml-shadow>";
const VALUE_METADATA: &str = "<ocaml-value-fence>";

pub(super) fn callee(
    node: Node<'_>,
    source: &str,
    language: SourceLanguage,
) -> Result<Option<ExtractedImportBinding>, ExtractError> {
    if language != SourceLanguage::Ocaml {
        return Ok(None);
    }
    let Some(path) = node.parent().filter(|path| path.kind() == "value_path") else {
        return Ok(None);
    };
    let name = source.get(path.byte_range()).unwrap_or_default();
    if !static_path(name) {
        return Ok(None);
    }
    Ok(Some(ExtractedImportBinding {
        kind: ImportBindingKind::Named,
        module_specifier: CALL_METADATA.to_owned(),
        imported_name: name.to_owned(),
        local_name: source.get(node.byte_range()).unwrap_or_default().to_owned(),
        span: span_for(node)?,
    }))
}

pub(super) fn extract(
    input: TagExtractionInput<'_, '_>,
    budget: &mut ExtractionBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<ExtractedImportBinding>, ExtractError> {
    let mut bindings = Vec::new();
    if input.snapshot.language() != SourceLanguage::Ocaml {
        return Ok(bindings);
    }
    if input.root.has_error() {
        module_bindings::push_binding(
            (&mut bindings, metadata(FENCE_METADATA, "*", input.root)?),
            budget,
        )?;
    }
    let limit = input
        .snapshot
        .source()
        .len()
        .saturating_mul(TAG_AST_NODES_PER_SOURCE_BYTE)
        .saturating_add(MINIMUM_TAG_AST_NODES)
        .min(MAXIMUM_TAG_AST_NODES);
    let mut cursor = input.root.walk();
    let mut visited = 0_usize;
    let mut depth = 0_usize;
    loop {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        visited = visited.checked_add(1).ok_or(ExtractError::OutputLimit)?;
        if visited > limit || depth > MAX_TAG_AST_DEPTH {
            return Err(ExtractError::NestingLimit);
        }
        if let Some(binding) = binding(cursor.node(), input.snapshot.source())? {
            module_bindings::push_binding((&mut bindings, binding), budget)?;
        }
        if !module_bindings::advance_cursor(&mut cursor, &mut depth) {
            return Ok(bindings);
        }
    }
}

fn binding(node: Node<'_>, source: &str) -> Result<Option<ExtractedImportBinding>, ExtractError> {
    if node.kind() == "value_pattern"
        || node.kind() == "value_name"
            && node.parent().is_some_and(|parent| {
                parent.kind() == "let_binding"
                    && parent.child_by_field_name("pattern") == Some(node)
            })
    {
        return Ok(Some(metadata(
            SHADOW_METADATA,
            source.get(node.byte_range()).unwrap_or("*"),
            node,
        )?));
    }
    let metadata = match node.kind() {
        "let_binding" => return value_fence(node, source),
        "open_module" => OPEN_METADATA,
        "include_module" | "local_open_expression" | "package_pattern" | "module_parameter" => {
            FENCE_METADATA
        }
        "module_binding" if !root_binding(node) || unsupported_module(node) => FENCE_METADATA,
        _ => return Ok(None),
    };
    Ok(Some(module_site(node, source, metadata)?))
}

fn value_fence(
    node: Node<'_>,
    source: &str,
) -> Result<Option<ExtractedImportBinding>, ExtractError> {
    let Some(pattern) = node.child_by_field_name("pattern") else {
        return Ok(Some(metadata(VALUE_METADATA, "*", node)?));
    };
    let name = source.get(pattern.byte_range()).unwrap_or("*");
    if matches!(name.trim(), "()" | "_") || pattern.kind() == "parenthesized_operator" {
        return Ok(None);
    }
    if pattern.kind() != "value_name" {
        return Ok(Some(metadata(VALUE_METADATA, "*", node)?));
    }
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| child.kind() == "parameter")
        || node
            .child_by_field_name("body")
            .is_some_and(|body| matches!(body.kind(), "fun_expression" | "function_expression"))
    {
        return Ok(None);
    }
    Ok(Some(metadata(VALUE_METADATA, name, node)?))
}

fn module_site(
    node: Node<'_>,
    source: &str,
    metadata: &str,
) -> Result<ExtractedImportBinding, ExtractError> {
    let target = node.child_by_field_name("module");
    let name = target
        .and_then(|target| source.get(target.byte_range()))
        .unwrap_or_default();
    let root_open = metadata == OPEN_METADATA
        && static_path(name)
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == "compilation_unit");
    let fence_name = if matches!(node.kind(), "module_binding" | "module_parameter") {
        node.named_child(0)
            .and_then(|node| source.get(node.byte_range()))
            .unwrap_or("*")
    } else {
        "*"
    };
    Ok(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: if root_open {
            OPEN_METADATA
        } else {
            FENCE_METADATA
        }
        .to_owned(),
        imported_name: if root_open { name } else { fence_name }.to_owned(),
        local_name: "*".to_owned(),
        span: span_for(node)?,
    })
}

fn metadata(
    kind: &str,
    name: &str,
    node: Node<'_>,
) -> Result<ExtractedImportBinding, ExtractError> {
    Ok(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: kind.to_owned(),
        imported_name: name.to_owned(),
        local_name: "*".to_owned(),
        span: span_for(node)?,
    })
}

fn root_binding(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "module_definition"
            && parent
                .parent()
                .is_some_and(|owner| owner.kind() == "compilation_unit")
    })
}

fn unsupported_module(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.child_by_field_name("module_type").is_some()
        || node
            .child_by_field_name("body")
            .is_none_or(|body| body.kind() != "structure")
        || node
            .named_children(&mut cursor)
            .any(|child| child.kind() == "module_parameter")
}

fn static_path(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PATH_BYTES
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_'".contains(&byte))
        })
}
