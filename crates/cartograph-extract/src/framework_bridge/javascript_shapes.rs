//! Exact syntax for registry arguments and module-level bridge interfaces.

use tree_sitter::Node;

use super::{
    ExtractError, FrameworkBuilder, MAX_BRIDGE_SCAN_BYTES, Quoted, REGISTRY_FACTORIES, SymbolKind,
    add_landmark, add_member_landmark, quoted_after,
};

const MAX_WRAPPERS: usize = 16;
pub(super) const MAX_ARGUMENT_NODES: usize = 64;

pub(super) fn spec_registry(builder: &FrameworkBuilder<'_, '_>, end: usize) -> bool {
    let Some(call) = call_at(builder, end) else {
        return false;
    };
    let Some(arguments) = call.child_by_field_name("type_arguments") else {
        return false;
    };
    if single_value(arguments).is_none_or(|argument| {
        argument.kind() != "type_identifier"
            || builder.source().get(argument.byte_range()) != Some("Spec")
    }) {
        return false;
    }
    let mut node = call;
    for _ in 0..MAX_WRAPPERS {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == "program" {
            return true;
        }
        if matches!(
            parent.kind(),
            "function_declaration"
                | "function_expression"
                | "arrow_function"
                | "statement_block"
                | "class"
                | "class_declaration"
                | "abstract_class_declaration"
                | "class_body"
        ) {
            return false;
        }
        node = parent;
    }
    false
}

pub(super) fn call_at<'tree>(
    builder: &FrameworkBuilder<'tree, '_>,
    end: usize,
) -> Option<Node<'tree>> {
    let mut node = builder
        .syntax_root()?
        .named_descendant_for_byte_range(end.checked_sub(1)?, end)?;
    for _ in 0..MAX_WRAPPERS {
        if node.kind() == "call_expression" {
            return Some(node);
        }
        if matches!(node.kind(), "string" | "template_string" | "comment") {
            return None;
        }
        node = node.parent()?;
    }
    None
}

pub(super) fn argument<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &'source str,
    end: usize,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let Some(call) = call_at(builder, end) else {
        return Ok(None);
    };
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(None);
    };
    builder
        .bridge
        .charge_work(function.end_byte() - function.start_byte())?;
    let name = source.get(function.byte_range()).unwrap_or_default();
    if !REGISTRY_FACTORIES.contains(&name) && name != "codegenNativeComponent" {
        return Ok(None);
    }
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Ok(None);
    };
    let Some(value) = first_value(arguments) else {
        return Ok(None);
    };
    let Some(value) = unwrap(value) else {
        return Ok(None);
    };
    if value.kind() != "string" || value.end_byte() - value.start_byte() > MAX_BRIDGE_SCAN_BYTES {
        return Ok(None);
    }
    builder
        .bridge
        .charge_work(value.end_byte() - value.start_byte())?;
    Ok(
        quoted_after(&source[..value.end_byte()], value.start_byte())
            .filter(|literal| literal.quote_end + 1 == value.end_byte()),
    )
}

pub(super) fn unwrap(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_WRAPPERS {
        if !matches!(
            node.kind(),
            "parenthesized_expression"
                | "as_expression"
                | "satisfies_expression"
                | "type_assertion"
                | "non_null_expression"
        ) {
            return Some(node);
        }
        node = first_value(node)?;
    }
    None
}

fn first_value(node: Node<'_>) -> Option<Node<'_>> {
    if node.named_child_count() > MAX_ARGUMENT_NODES {
        return None;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| !child.is_extra() && child.kind() != "type_arguments")
}

fn single_value(node: Node<'_>) -> Option<Node<'_>> {
    if node.named_child_count() > MAX_ARGUMENT_NODES {
        return None;
    }
    let mut cursor = node.walk();
    let mut values = node
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra());
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

pub(super) fn declaration<'tree>(
    builder: &mut FrameworkBuilder<'tree, '_>,
    name: &str,
) -> Result<Option<Node<'tree>>, ExtractError> {
    let Some(root) = builder.syntax_root() else {
        return Ok(None);
    };
    let mut selected = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        let node = if child.kind() == "export_statement" {
            child.child_by_field_name("declaration").unwrap_or(child)
        } else {
            child
        };
        if !matches!(
            node.kind(),
            "interface_declaration" | "type_alias_declaration"
        ) {
            continue;
        }
        let Some(identifier) = node.child_by_field_name("name") else {
            continue;
        };
        if builder.source().get(identifier.byte_range()) == Some(name) {
            if selected.is_some() {
                return Ok(None);
            }
            selected = Some(node);
        }
    }
    Ok(selected)
}

pub(super) fn members(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    query: (&str, Option<&str>),
) -> Result<(), ExtractError> {
    let (name, module) = query;
    let Some(declaration) = declaration(builder, name)? else {
        return Ok(());
    };
    let Some(body) = object_body(builder, declaration) else {
        return Ok(());
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        append_member(builder, source, (member, module))?;
    }
    Ok(())
}

fn append_member(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (member, module): (Node<'_>, Option<&str>),
) -> Result<(), ExtractError> {
    let expected = if module.is_some() {
        "method_signature"
    } else {
        "property_signature"
    };
    if member.kind() != expected {
        return Ok(());
    }
    let Some(identifier) = member
        .child_by_field_name("name")
        .filter(|node| node.kind() == "property_identifier")
    else {
        return Ok(());
    };
    let Some(value) = source.get(identifier.byte_range()) else {
        return Ok(());
    };
    let site = (value, identifier.start_byte(), identifier.end_byte());
    if let Some(module) = module {
        add_member_landmark(
            builder,
            (SymbolKind::Method, "turbo-module-spec-method", module),
            site,
        )?;
    } else {
        add_landmark(builder, (SymbolKind::Property, "fabric-prop"), site)?;
    }
    Ok(())
}

fn object_body<'tree>(
    builder: &FrameworkBuilder<'_, '_>,
    declaration: Node<'tree>,
) -> Option<Node<'tree>> {
    if declaration.kind() == "interface_declaration" {
        return declaration.child_by_field_name("body");
    }
    let value = declaration.child_by_field_name("value")?;
    if value.kind() == "object_type" {
        return Some(value);
    }
    if value.kind() != "generic_type" {
        return None;
    }
    let name = value.child_by_field_name("name")?;
    if builder.source().get(name.byte_range()) != Some("Readonly") {
        return None;
    }
    let arguments = value.child_by_field_name("type_arguments")?;
    single_value(arguments).filter(|node| node.kind() == "object_type")
}

pub(super) fn assigned_alias<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &'source str,
    end: usize,
) -> Result<Option<&'source str>, ExtractError> {
    let Some(call) = call_at(builder, end) else {
        return Ok(None);
    };
    let mut node = call;
    for _ in 0..MAX_WRAPPERS {
        builder.bridge.charge_work(1)?;
        let Some(parent) = node.parent() else {
            return Ok(None);
        };
        if parent.kind() == "variable_declarator" {
            if !module_declarator(parent) {
                return Ok(None);
            }
            let Some(value) = parent.child_by_field_name("value").and_then(unwrap) else {
                return Ok(None);
            };
            if value.id() != call.id() {
                return Ok(None);
            }
            return Ok(parent
                .child_by_field_name("name")
                .filter(|name| name.kind() == "identifier")
                .and_then(|name| source.get(name.byte_range())));
        }
        if !matches!(
            parent.kind(),
            "parenthesized_expression" | "as_expression" | "type_assertion" | "non_null_expression"
        ) {
            return Ok(None);
        }
        node = parent;
    }
    Ok(None)
}

pub(super) fn module_declarator(mut node: Node<'_>) -> bool {
    for _ in 0..MAX_WRAPPERS {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == "program" {
            return true;
        }
        if !matches!(
            parent.kind(),
            "lexical_declaration" | "variable_declaration" | "export_statement"
        ) {
            return false;
        }
        node = parent;
    }
    false
}
