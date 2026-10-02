use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{
    ExtractError, ExtractedReference,
    walk::{
        ExtractionBuilder, ExtractionContext, JoinedSignature, PendingSymbol, joined_signature,
        syntax::{DescendantQuery, descendant_of_kind, named_children, span_for},
    },
};

const MAX_TYPE_DEPTH: usize = 32;
const MAX_RELATION_TARGETS: usize = 1024;

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    _depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "type_definition" => {
            if let Some(definition) = first_supported_definition(node) {
                emit_definition(builder, definition, false)?;
            }
            Ok(true)
        }
        "type_extension" => {
            if let Some(extension) = first_supported_extension(node) {
                emit_definition(builder, extension, true)?;
            }
            Ok(true)
        }
        "directive_definition" => {
            emit_directive(builder, node)?;
            Ok(true)
        }
        "schema_definition"
        | "executable_definition"
        | "operation_definition"
        | "fragment_definition" => Ok(true),
        kind if is_supported_definition(kind) => {
            emit_definition(builder, node, false)?;
            Ok(true)
        }
        kind if is_supported_extension(kind) => {
            emit_definition(builder, node, true)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(super) fn description_from_context(
    context: &mut ExtractionContext<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(description) = direct_child(node, "description") else {
        return Ok(None);
    };
    let raw = context.text(description).trim();
    let stripped = raw
        .strip_circumfix("\"\"\"", "\"\"\"")
        .or_else(|| raw.strip_circumfix('"', '"'))
        .unwrap_or(raw)
        .trim();
    if stripped.is_empty() {
        Ok(None)
    } else {
        context.copy_text(stripped).map(Some)
    }
}

fn first_supported_definition(node: Node<'_>) -> Option<Node<'_>> {
    direct_named_children(node).find(|child| is_supported_definition(child.kind()))
}

fn first_supported_extension(node: Node<'_>) -> Option<Node<'_>> {
    direct_named_children(node).find(|child| is_supported_extension(child.kind()))
}

fn is_supported_definition(kind: &str) -> bool {
    matches!(
        kind,
        "object_type_definition"
            | "interface_type_definition"
            | "input_object_type_definition"
            | "enum_type_definition"
            | "union_type_definition"
            | "scalar_type_definition"
    )
}

fn is_supported_extension(kind: &str) -> bool {
    matches!(
        kind,
        "object_type_extension"
            | "interface_type_extension"
            | "input_object_type_extension"
            | "enum_type_extension"
            | "union_type_extension"
            | "scalar_type_extension"
    )
}

fn emit_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    extension: bool,
) -> Result<(), ExtractError> {
    match node.kind() {
        "object_type_definition" | "object_type_extension" => emit_fields_container(
            builder,
            FieldsContainerInput {
                node,
                extension,
                kind: SymbolKind::Class,
                keyword: "type",
                block_kind: "fields_definition",
            },
        ),
        "interface_type_definition" | "interface_type_extension" => emit_fields_container(
            builder,
            FieldsContainerInput {
                node,
                extension,
                kind: SymbolKind::Interface,
                keyword: "interface",
                block_kind: "fields_definition",
            },
        ),
        "input_object_type_definition" | "input_object_type_extension" => emit_fields_container(
            builder,
            FieldsContainerInput {
                node,
                extension,
                kind: SymbolKind::Class,
                keyword: "input",
                block_kind: "input_fields_definition",
            },
        ),
        "enum_type_definition" | "enum_type_extension" => emit_enum(builder, node, extension),
        "union_type_definition" | "union_type_extension" => emit_union(builder, node, extension),
        "scalar_type_definition" | "scalar_type_extension" => emit_scalar(builder, node, extension),
        _ => Ok(()),
    }
}

#[derive(Clone, Copy)]
struct FieldsContainerInput<'tree> {
    node: Node<'tree>,
    extension: bool,
    kind: SymbolKind,
    keyword: &'static str,
    block_kind: &'static str,
}

fn emit_fields_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: FieldsContainerInput<'_>,
) -> Result<(), ExtractError> {
    let FieldsContainerInput {
        node,
        extension,
        kind,
        keyword,
        block_kind,
    } = input;
    let Some(name_node) = direct_child(node, "name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let signature = declaration_signature(
        builder,
        DeclarationSignature {
            extension,
            keyword,
            name: &name,
        },
    )?;
    let pending = PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: Some(signature),
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    let owner = builder.emit_symbol(pending)?;
    if extension {
        emit_reference(
            builder,
            GraphqlReference {
                owner: &owner,
                node: name_node,
                name: &name,
                kind: ReferenceKind::Extends,
            },
        )?;
    }
    emit_named_relations(
        builder,
        NamedRelations {
            node,
            container_kind: "implements_interfaces",
            owner: &owner,
            kind: ReferenceKind::Implements,
        },
    )?;

    builder.owners.push(owner);
    builder.native_owner_kinds.push(kind);
    builder.native_visibilities.push(None);
    builder.qualifiers.push(name);
    if let Some(fields) = direct_child(node, block_kind) {
        for field in direct_named_children(fields) {
            builder.context.ensure_active()?;
            if matches!(field.kind(), "field_definition" | "input_value_definition") {
                emit_field(builder, field)?;
            }
        }
    }
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    Ok(())
}

fn emit_field(builder: &mut ExtractionBuilder<'_, '_>, node: Node<'_>) -> Result<(), ExtractError> {
    let Some(name_node) = direct_child(node, "name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let type_node = direct_child(node, "type");
    let signature = match type_node {
        Some(type_node) => Some(field_signature(builder, &name, type_node)?),
        None => Some(builder.context.copy_text(&name)?),
    };
    let pending = PendingSymbol {
        kind: SymbolKind::Field,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature,
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    let field = builder.emit_symbol(pending)?;
    if let Some(type_node) = type_node
        && let Some((base_node, base_name)) = base_named_type(builder, type_node, 0)?
        && !is_builtin(&base_name)
    {
        emit_reference(
            builder,
            GraphqlReference {
                owner: &field,
                node: base_node,
                name: &base_name,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    Ok(())
}

fn emit_enum(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    extension: bool,
) -> Result<(), ExtractError> {
    let Some(name_node) = direct_child(node, "name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let signature = declaration_signature(
        builder,
        DeclarationSignature {
            extension,
            keyword: "enum",
            name: &name,
        },
    )?;
    let pending = PendingSymbol {
        kind: SymbolKind::Enum,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: Some(signature),
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    let owner = builder.emit_symbol(pending)?;
    if extension {
        emit_reference(
            builder,
            GraphqlReference {
                owner: &owner,
                node: name_node,
                name: &name,
                kind: ReferenceKind::Extends,
            },
        )?;
    }
    builder.owners.push(owner);
    builder.native_owner_kinds.push(SymbolKind::Enum);
    builder.native_visibilities.push(None);
    builder.qualifiers.push(name.clone());
    if let Some(values) = direct_child(node, "enum_values_definition") {
        for value in direct_named_children(values) {
            builder.context.ensure_active()?;
            if value.kind() != "enum_value_definition" {
                continue;
            }
            let Some(value_node) = descendant_of_kind(DescendantQuery {
                node: value,
                kind: "name",
                depth: 0,
                maximum_depth: MAX_TYPE_DEPTH,
            }) else {
                continue;
            };
            let member = builder.context.owned_text(value_node)?;
            let signature = joined_signature(builder, JoinedSignature::dotted(&name, &member))?;
            builder.emit_symbol(PendingSymbol {
                kind: SymbolKind::EnumMember,
                name: member,
                span_node: value,
                structural_node: value,
                doc_anchor: value,
                body_node: None,
                declaration_only: false,
                signature: Some(signature),
                export: crate::SymbolExportFlags::new(false, false),
                async_symbol: false,
                static_member: false,
                visibility: None,
            })?;
        }
    }
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    Ok(())
}

fn emit_union(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    extension: bool,
) -> Result<(), ExtractError> {
    let Some(name_node) = direct_child(node, "name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let signature = declaration_signature(
        builder,
        DeclarationSignature {
            extension,
            keyword: "union",
            name: &name,
        },
    )?;
    let owner = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::TypeAlias,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: Some(signature),
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    if extension {
        emit_reference(
            builder,
            GraphqlReference {
                owner: &owner,
                node: name_node,
                name: &name,
                kind: ReferenceKind::Extends,
            },
        )?;
    }
    emit_named_relations(
        builder,
        NamedRelations {
            node,
            container_kind: "union_member_types",
            owner: &owner,
            kind: ReferenceKind::References,
        },
    )
}

fn emit_scalar(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    extension: bool,
) -> Result<(), ExtractError> {
    let Some(name_node) = direct_child(node, "name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let signature = declaration_signature(
        builder,
        DeclarationSignature {
            extension,
            keyword: "scalar",
            name: &name,
        },
    )?;
    let owner = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::TypeAlias,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: Some(signature),
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    if extension {
        emit_reference(
            builder,
            GraphqlReference {
                owner: &owner,
                node: name_node,
                name: &name,
                kind: ReferenceKind::Extends,
            },
        )?;
    }
    Ok(())
}

fn emit_directive(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = direct_child(node, "name") else {
        return Ok(());
    };
    let raw_name = builder.context.owned_text(name_node)?;
    let name = prefixed_name(builder, '@', &raw_name)?;
    let signature = directive_signature(builder, node, &name)?;
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Function,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: true,
        signature: Some(signature),
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    Ok(())
}

#[derive(Clone, Copy)]
struct NamedRelations<'tree, 'owner> {
    node: Node<'tree>,
    container_kind: &'static str,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
}

fn emit_named_relations(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamedRelations<'_, '_>,
) -> Result<(), ExtractError> {
    let Some(container) = direct_child(input.node, input.container_kind) else {
        return Ok(());
    };
    let mut targets = Vec::new();
    collect_named_types(container, 0, &mut targets)?;
    if targets.len() > MAX_RELATION_TARGETS {
        return Err(ExtractError::OutputLimit);
    }
    let mut seen = BTreeSet::new();
    for target in targets {
        builder.context.ensure_active()?;
        let Some(name_node) = direct_child(target, "name") else {
            continue;
        };
        let name = builder.context.owned_text(name_node)?;
        if seen.insert(name.clone()) {
            emit_reference(
                builder,
                GraphqlReference {
                    owner: input.owner,
                    node: name_node,
                    name: &name,
                    kind: input.kind,
                },
            )?;
        }
    }
    Ok(())
}

fn collect_named_types<'tree>(
    node: Node<'tree>,
    depth: usize,
    output: &mut Vec<Node<'tree>>,
) -> Result<(), ExtractError> {
    if depth > MAX_TYPE_DEPTH {
        return Err(ExtractError::NestingLimit);
    }
    if node.kind() == "named_type" {
        output.push(node);
        if output.len() > MAX_RELATION_TARGETS {
            return Err(ExtractError::OutputLimit);
        }
        return Ok(());
    }
    for child in direct_named_children(node) {
        collect_named_types(child, depth.saturating_add(1), output)?;
    }
    Ok(())
}

fn base_named_type<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'tree>,
    depth: usize,
) -> Result<Option<(Node<'tree>, String)>, ExtractError> {
    if depth > MAX_TYPE_DEPTH {
        return Err(ExtractError::NestingLimit);
    }
    if node.kind() == "named_type" {
        let Some(name_node) = direct_child(node, "name") else {
            return Ok(None);
        };
        let name = builder.context.owned_text(name_node)?;
        return Ok(Some((name_node, name)));
    }
    for child in direct_named_children(node) {
        if let Some(found) = base_named_type(builder, child, depth.saturating_add(1))? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy)]
struct GraphqlReference<'tree, 'value> {
    owner: &'value SymbolId,
    node: Node<'tree>,
    name: &'value str,
    kind: ReferenceKind,
}

fn emit_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: GraphqlReference<'_, '_>,
) -> Result<(), ExtractError> {
    builder.emit_reference(ExtractedReference {
        owner: Some(input.owner.clone()),
        name: builder.context.copy_text(input.name)?,
        resolution_name: None,
        kind: input.kind,
        span: span_for(input.node)?,
    })
}

#[derive(Clone, Copy)]
struct DeclarationSignature<'value> {
    extension: bool,
    keyword: &'value str,
    name: &'value str,
}

fn declaration_signature(
    builder: &ExtractionBuilder<'_, '_>,
    input: DeclarationSignature<'_>,
) -> Result<String, ExtractError> {
    let prefix = if input.extension { "extend " } else { "" };
    let length = prefix
        .len()
        .checked_add(input.keyword.len())
        .and_then(|length| length.checked_add(input.name.len()))
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(prefix);
    signature.push_str(input.keyword);
    signature.push(' ');
    signature.push_str(input.name);
    builder.context.copy_text(&signature)
}

fn field_signature(
    builder: &ExtractionBuilder<'_, '_>,
    name: &str,
    type_node: Node<'_>,
) -> Result<String, ExtractError> {
    let type_text = builder.context.text(type_node).trim();
    let length = name
        .len()
        .checked_add(type_text.len())
        .and_then(|length| length.checked_add(2))
        .ok_or(ExtractError::OutputLimit)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(name);
    signature.push_str(": ");
    signature.push_str(type_text);
    builder.context.copy_text(&signature)
}

fn directive_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: &str,
) -> Result<String, ExtractError> {
    let mut signature = declaration_signature(
        builder,
        DeclarationSignature {
            extension: false,
            keyword: "directive",
            name,
        },
    )?;
    let Some(arguments) = direct_child(node, "arguments_definition") else {
        return Ok(signature);
    };
    let mut rendered = Vec::new();
    for argument in direct_named_children(arguments) {
        builder.context.ensure_active()?;
        if argument.kind() != "input_value_definition" {
            continue;
        }
        let Some(argument_name) = direct_child(argument, "name") else {
            continue;
        };
        let name = builder.context.owned_text(argument_name)?;
        let Some(type_node) = direct_child(argument, "type") else {
            continue;
        };
        rendered.push(field_signature(builder, &name, type_node)?);
    }
    if rendered.is_empty() {
        return Ok(signature);
    }
    let added = rendered.iter().try_fold(2_usize, |length, argument| {
        length
            .checked_add(argument.len())
            .and_then(|length| length.checked_add(2))
    });
    signature
        .try_reserve(added.ok_or(ExtractError::OutputLimit)?)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push('(');
    for (index, argument) in rendered.iter().enumerate() {
        if index > 0 {
            signature.push_str(", ");
        }
        signature.push_str(argument);
    }
    signature.push(')');
    builder.context.copy_text(&signature)
}

fn prefixed_name(
    builder: &ExtractionBuilder<'_, '_>,
    prefix: char,
    name: &str,
) -> Result<String, ExtractError> {
    let mut output = String::new();
    output
        .try_reserve(name.len().saturating_add(prefix.len_utf8()))
        .map_err(|_| ExtractError::OutputLimit)?;
    output.push(prefix);
    output.push_str(name);
    builder.context.copy_text(&output)
}

fn is_builtin(name: &str) -> bool {
    matches!(name, "Int" | "Float" | "String" | "Boolean" | "ID")
}

fn direct_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    direct_named_children(node).find(|child| child.kind() == kind)
}

fn direct_named_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    named_children(node)
}
