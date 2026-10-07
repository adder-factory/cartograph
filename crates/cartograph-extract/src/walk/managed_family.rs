use cartograph_domain::{
    ReferenceKind, SourceLanguage, SymbolId, SymbolKind, Visibility,
    callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};

use super::{
    ChildReferenceKind, ExtractionBuilder, PendingReference, PendingSymbol,
    capture_child_reference, references,
    syntax::{children, descendants_including_root, named_children, span_for},
    with_root_scope,
};

const MAX_SIGNATURE_BYTES: usize = 512;
const MAX_REFERENCE_TARGET_BYTES: usize = 512;
const MAX_TYPE_DEPTH: usize = 64;
/// Longest type text whose nested generic arguments are stripped in place.
const MAX_NESTED_GENERIC_BYTES: usize = 4_096;

struct ImportEmission<'tree, 'text> {
    node: Node<'tree>,
    name: String,
    raw_signature: &'text str,
}

#[derive(Clone, Copy)]
struct BindingEmission<'tree, 'text> {
    node: Node<'tree>,
    kind: ImportBindingKind,
    module: &'text str,
    imported: &'text str,
    local: &'text str,
}

#[derive(Clone, Copy)]
struct PrimaryConstructor<'tree, 'text> {
    parameters: Node<'tree>,
    name: &'text str,
    visibility: Option<Visibility>,
}

#[derive(Clone, Copy)]
struct InheritanceCapture<'tree, 'owner> {
    node: Node<'tree>,
    owner: &'owner SymbolId,
    kind: SymbolKind,
    name_node: Node<'tree>,
}

#[derive(Clone, Copy)]
struct VariableVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
}

#[derive(Clone, Copy)]
struct TypeReferenceCapture<'tree, 'owner> {
    node: Node<'tree>,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
    depth: usize,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    super::jvm_type_lookup::record_java_local_type(builder, node)?;
    match node.kind() {
        "package_declaration" | "file_scoped_namespace_declaration" => {
            visit_persistent_namespace(builder, node)?;
        }
        "namespace_declaration" => visit_scoped_namespace(builder, node, depth)?,
        "import_declaration" => visit_java_import(builder, node)?,
        "using_directive" => visit_csharp_using(builder, node)?,
        "class_declaration"
        | "interface_declaration"
        | "record_declaration"
        | "struct_declaration"
        | "enum_declaration"
        | "annotation_type_declaration" => visit_container(builder, node, depth)?,
        "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" => {
            visit_callable(builder, node, depth)?;
        }
        "annotation_type_element_declaration" | "property_declaration" => {
            visit_property(builder, node, depth)?;
        }
        "field_declaration" | "event_field_declaration" | "constant_declaration" => {
            visit_variables(
                builder,
                VariableVisit {
                    node,
                    depth,
                    kind: SymbolKind::Field,
                },
            )?;
        }
        "local_variable_declaration" | "local_declaration_statement" => {
            visit_variables(
                builder,
                VariableVisit {
                    node,
                    depth,
                    kind: SymbolKind::Variable,
                },
            )?;
        }
        "enum_constant" | "enum_member_declaration" => visit_enum_member(builder, node)?,
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "method_invocation" | "invocation_expression" => capture_call(builder, node),
        "object_creation_expression" => {
            capture_child_reference(builder, node, ChildReferenceKind::ManagedConstruction)
        }
        "field_access" | "member_access_expression" => capture_member_access(builder, node),
        _ => Ok(()),
    }
}

fn visit_persistent_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = namespace_name_node(node) else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let id = emit_namespace(builder, node, name.clone())?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Namespace);
    builder.qualifiers.push(name);
    Ok(())
}

fn visit_scoped_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = namespace_name_node(node) else {
        return builder.visit_named_children(node, depth);
    };
    let Some(body) = node.child_by_field_name("body") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let id = emit_namespace(builder, node, name.clone())?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Namespace);
    builder.qualifiers.push(name);
    let result = builder.visit(body, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn namespace_name_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("name").or_else(|| {
        named_children(node).find(|child| {
            matches!(
                child.kind(),
                "identifier" | "scoped_identifier" | "qualified_name" | "alias_qualified_name"
            )
        })
    })
}

fn emit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: String,
) -> Result<SymbolId, ExtractError> {
    builder.emit_symbol(PendingSymbol::namespace(node, name))
}

fn visit_java_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let raw = builder.context.owned_text(node)?;
    let Some(parsed) = parse_java_import(&raw) else {
        return Ok(());
    };
    let module_name = builder.context.copy_text(parsed.target)?;
    emit_import(
        builder,
        ImportEmission {
            node,
            name: module_name.clone(),
            raw_signature: &raw,
        },
    )?;
    if parsed.wildcard {
        emit_binding(
            builder,
            BindingEmission {
                node,
                kind: ImportBindingKind::Namespace,
                module: parsed.target,
                imported: "*",
                local: "*",
            },
        )?;
    } else if let Some((module, imported)) = parsed.target.rsplit_once('.') {
        emit_binding(
            builder,
            BindingEmission {
                node,
                kind: ImportBindingKind::Named,
                module,
                imported,
                local: imported,
            },
        )?;
    }
    Ok(())
}

struct JavaImport<'source> {
    target: &'source str,
    wildcard: bool,
}

fn parse_java_import(raw: &str) -> Option<JavaImport<'_>> {
    let mut body = strip_keyword(raw, "import")?;
    body = strip_keyword(body, "static").unwrap_or(body);
    body = body.strip_suffix(';').unwrap_or(body).trim();
    let wildcard = body.ends_with(".*");
    let target = body.strip_suffix(".*").unwrap_or(body).trim();
    safe_import_target(target).then_some(JavaImport { target, wildcard })
}

fn visit_csharp_using(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let raw = builder.context.owned_text(node)?;
    let Some(parsed) = parse_csharp_using(&raw) else {
        return Ok(());
    };
    let module_name = builder.context.copy_text(parsed.target)?;
    emit_import(
        builder,
        ImportEmission {
            node,
            name: module_name,
            raw_signature: &raw,
        },
    )?;
    if let Some(alias) = parsed.alias {
        emit_binding(
            builder,
            BindingEmission {
                node,
                kind: ImportBindingKind::Namespace,
                module: parsed.target,
                imported: "*",
                local: alias,
            },
        )?;
    } else if parsed.static_import {
        emit_binding(
            builder,
            BindingEmission {
                node,
                kind: ImportBindingKind::Namespace,
                module: parsed.target,
                imported: "*",
                local: "*",
            },
        )?;
    }
    Ok(())
}

struct CsharpUsing<'source> {
    target: &'source str,
    alias: Option<&'source str>,
    static_import: bool,
}

fn parse_csharp_using(raw: &str) -> Option<CsharpUsing<'_>> {
    let mut body = strip_keyword(raw, "global").unwrap_or(raw.trim());
    body = strip_keyword(body, "using")?;
    body = body.strip_suffix(';').unwrap_or(body).trim();
    let static_body = strip_keyword(body, "static");
    let static_import = static_body.is_some();
    body = static_body.unwrap_or(body);
    let (alias, target) = body
        .split_once('=')
        .map_or((None, body), |(alias, target)| {
            (Some(alias.trim()), target.trim())
        });
    if !safe_import_target(target) || alias.is_some_and(|alias| !safe_identifier(alias)) {
        return None;
    }
    Some(CsharpUsing {
        target,
        alias,
        static_import,
    })
}

fn strip_keyword<'source>(value: &'source str, keyword: &str) -> Option<&'source str> {
    let value = value.trim_start();
    let suffix = value.strip_prefix(keyword)?;
    suffix
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| suffix.trim_start())
}

fn safe_import_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= MAX_REFERENCE_TARGET_BYTES
        && !target
            .bytes()
            .any(|byte| matches!(byte, b'\'' | b'"' | b'`' | b'=' | b';'))
        && target.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'.' | b':' | b'$' | b'<' | b'>' | b',' | b'?' | b'[' | b']' | b' '
                )
        })
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn emit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ImportEmission<'_, '_>,
) -> Result<(), ExtractError> {
    let signature = if input.raw_signature.len() <= MAX_SIGNATURE_BYTES {
        Some(builder.context.copy_text(input.raw_signature)?)
    } else {
        None
    };
    let reference_name = input.name.clone();
    emit_root_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Import,
            name: input.name,
            span_node: input.node,
            structural_node: input.node,
            doc_anchor: input.node,
            body_node: None,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        },
    )?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: reference_name,
            kind: ReferenceKind::Imports,
            node: input.node,
        },
    )
}

fn emit_root_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: PendingSymbol<'_>,
) -> Result<SymbolId, ExtractError> {
    with_root_scope(builder, |builder| builder.emit_symbol(pending))
}

fn emit_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: BindingEmission<'_, '_>,
) -> Result<(), ExtractError> {
    builder.emit_import_binding(ExtractedImportBinding {
        kind: input.kind,
        module_specifier: builder.context.copy_text(input.module)?,
        imported_name: builder.context.copy_text(input.imported)?,
        local_name: builder.context.copy_text(input.local)?,
        span: span_for(input.node)?,
    })
}

fn visit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        "interface_declaration" | "annotation_type_declaration" => SymbolKind::Interface,
        "record_declaration" | "struct_declaration" => SymbolKind::Struct,
        "enum_declaration" => SymbolKind::Enum,
        _ => return builder.visit_named_children(node, depth),
    };
    let name = builder.context.owned_text(name_node)?;
    let modifiers = managed_modifiers(builder, node)?;
    let visibility = managed_visibility(builder, modifiers, true);
    let body = node.child_by_field_name("body");
    let pending = PendingSymbol {
        kind,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
        async_symbol: false,
        static_member: modifiers.contains(ManagedModifier::Static)
            || (builder.context.snapshot.language() == SourceLanguage::Java
                && matches!(
                    builder.native_owner_kinds.last(),
                    Some(SymbolKind::Interface)
                )),
        visibility,
    };
    let id = builder.emit_symbol(pending)?;
    capture_decorators(builder, node, &id)?;
    capture_inheritance(
        builder,
        InheritanceCapture {
            node,
            owner: &id,
            kind,
            name_node,
        },
    )?;

    builder.owners.push(id);
    builder.native_owner_kinds.push(kind);
    builder.qualifiers.push(name.clone());
    if let Some(parameters) = managed_parameters(node)
        && matches!(
            node.kind(),
            "record_declaration" | "class_declaration" | "struct_declaration"
        )
    {
        emit_primary_constructor(
            builder,
            PrimaryConstructor {
                parameters,
                name: &name,
                visibility,
            },
        )?;
    }
    let result = match body {
        Some(body) => builder.visit(body, depth.saturating_add(1)),
        None => Ok(()),
    };
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn emit_primary_constructor(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PrimaryConstructor<'_, '_>,
) -> Result<(), ExtractError> {
    let signature = managed_signature(builder, None, input.parameters)?;
    let pending = PendingSymbol {
        kind: SymbolKind::Method,
        name: builder.context.copy_text(input.name)?,
        span_node: input.parameters,
        structural_node: input.parameters,
        doc_anchor: input.parameters,
        body_node: None,
        declaration_only: false,
        signature,
        export: crate::SymbolExportFlags::new(input.visibility == Some(Visibility::Public), false),
        async_symbol: false,
        static_member: false,
        visibility: input.visibility,
    };
    let id = builder.emit_symbol(pending)?;
    capture_parameter_types(builder, input.parameters, &id)
}

fn capture_inheritance(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: InheritanceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Java | SourceLanguage::Apex => capture_java_inheritance(builder, input)?,
        SourceLanguage::CSharp => capture_csharp_inheritance(builder, input)?,
        _ => {}
    }
    Ok(())
}

fn capture_java_inheritance(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: InheritanceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    if let Some(superclass) = input.node.child_by_field_name("superclass") {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: superclass,
                owner: input.owner,
                kind: ReferenceKind::Extends,
                depth: 0,
            },
        )?;
    }
    let interfaces_kind = if input.kind == SymbolKind::Interface {
        ReferenceKind::Extends
    } else {
        ReferenceKind::Implements
    };
    if let Some(interfaces) = input.node.child_by_field_name("interfaces") {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: interfaces,
                owner: input.owner,
                kind: interfaces_kind,
                depth: 0,
            },
        )?;
    }
    for child in named_children(input.node).filter(|child| child.kind() == "extends_interfaces") {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: child,
                owner: input.owner,
                kind: ReferenceKind::Extends,
                depth: 0,
            },
        )?;
    }
    Ok(())
}

fn capture_csharp_inheritance(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: InheritanceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let Some(base_list) = named_children(input.node).find(|child| child.kind() == "base_list")
    else {
        return Ok(());
    };
    let value_type = input.node.kind() == "struct_declaration"
        || (input.node.kind() == "record_declaration"
            && csharp_record_is_struct(builder, input.node, input.name_node)?);
    for target in named_children(base_list) {
        builder.context.ensure_active()?;
        if target.kind() == "argument_list" {
            continue;
        }
        let Some((target_name, target_node)) = csharp_base_target(builder, target)? else {
            continue;
        };
        let reference_kind = if input.kind == SymbolKind::Interface {
            ReferenceKind::Extends
        } else if value_type {
            ReferenceKind::Implements
        } else {
            ReferenceKind::Inherits
        };
        push_named_reference(
            builder,
            PendingReference {
                owner: Some(input.owner.clone()),
                name: target_name,
                kind: reference_kind,
                node: target_node,
            },
        )?;
    }
    Ok(())
}

fn csharp_base_target<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'tree>,
) -> Result<Option<(String, Node<'tree>)>, ExtractError> {
    let target_node = if target.kind() == "primary_constructor_base_type" {
        let Some(type_node) = named_children(target).find(|child| child.kind() != "argument_list")
        else {
            return Ok(None);
        };
        type_node
    } else {
        target
    };
    let Some(name) = managed_outer_type_name(builder, target_node)? else {
        return Ok(None);
    };
    Ok(Some((name, target_node)))
}

fn csharp_record_is_struct(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: Node<'_>,
) -> Result<bool, ExtractError> {
    for child in children(node) {
        builder.context.ensure_active()?;
        if child.start_byte() >= name.start_byte() {
            break;
        }
        if child.kind() == "struct" || builder.context.text(child).trim() == "struct" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(parameters) = managed_parameters(node) else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let return_type = managed_return_type(node);
    let body = node.child_by_field_name("body");
    let modifiers = managed_modifiers(builder, node)?;
    let visibility = managed_visibility(builder, modifiers, false);
    let pending = PendingSymbol {
        kind: SymbolKind::Method,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only: body.is_none(),
        signature: managed_signature(builder, return_type, parameters)?,
        export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
        async_symbol: modifiers.contains(ManagedModifier::Async),
        static_member: modifiers.contains(ManagedModifier::Static),
        visibility,
    };
    let id = builder.emit_symbol(pending)?;
    capture_decorators(builder, node, &id)?;
    capture_parameter_types(builder, parameters, &id)?;
    if let Some(return_type) = return_type {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: return_type,
                owner: &id,
                kind: ReferenceKind::Returns,
                depth: 0,
            },
        )?;
    }

    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Method);
    builder.qualifiers.push(name);
    emit_annotated_parameters(builder, parameters)?;
    let result = match body {
        Some(body) => builder.visit(body, depth.saturating_add(1)),
        None => Ok(()),
    };
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn managed_parameters(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("parameters").or_else(|| {
        named_children(node)
            .find(|child| matches!(child.kind(), "formal_parameters" | "parameter_list"))
    })
}

fn managed_return_type(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(
        node.kind(),
        "constructor_declaration" | "compact_constructor_declaration"
    ) {
        return None;
    }
    node.child_by_field_name("type")
        .or_else(|| node.child_by_field_name("returns"))
}

fn managed_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    return_type: Option<Node<'_>>,
    parameters: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let parameter_text = builder.context.text(parameters).trim();
    let return_text = return_type
        .map(|node| builder.context.text(node).trim())
        .filter(|value| !value.is_empty());
    let length = parameter_text
        .len()
        .checked_add(return_text.map_or(0, |value| value.len().saturating_add(1)))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        return Ok(None);
    }
    builder.context.budget.ensure_string_length(length)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    if let Some(return_text) = return_text {
        signature.push_str(return_text);
        signature.push(' ');
    }
    signature.push_str(parameter_text);
    if !callable_signature_is_literal_free(&signature) {
        return Ok(None);
    }
    Ok(Some(signature))
}

fn capture_parameter_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for parameter in named_children(parameters) {
        builder.context.ensure_active()?;
        if let Some(type_node) = parameter.child_by_field_name("type") {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    node: type_node,
                    owner,
                    kind: ReferenceKind::TypeOf,
                    depth: 0,
                },
            )?;
        }
    }
    Ok(())
}

fn emit_annotated_parameters(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
) -> Result<(), ExtractError> {
    for parameter in named_children(parameters) {
        builder.context.ensure_active()?;
        if !managed_modifiers(builder, parameter)?.contains(ManagedModifier::Decorated) {
            continue;
        }
        let Some(name_node) = parameter.child_by_field_name("name") else {
            continue;
        };
        let name = builder.context.owned_text(name_node)?;
        let type_node = parameter.child_by_field_name("type");
        let signature = managed_typed_name(builder, type_node, &name)?;
        let pending = PendingSymbol {
            kind: SymbolKind::Parameter,
            name,
            span_node: parameter,
            structural_node: parameter,
            doc_anchor: parameter,
            body_node: None,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        };
        let id = builder.emit_symbol(pending)?;
        capture_decorators(builder, parameter, &id)?;
        if let Some(type_node) = type_node {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    node: type_node,
                    owner: &id,
                    kind: ReferenceKind::TypeOf,
                    depth: 0,
                },
            )?;
        }
    }
    Ok(())
}

fn visit_property(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let type_node = node.child_by_field_name("type");
    let modifiers = managed_modifiers(builder, node)?;
    let visibility = managed_visibility(builder, modifiers, false);
    let body = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("accessors"));
    let declaration_only = matches!(
        builder.native_owner_kinds.last(),
        Some(SymbolKind::Interface)
    );
    let pending = PendingSymbol {
        kind: SymbolKind::Property,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only,
        signature: managed_typed_name(builder, type_node, &name)?,
        export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
        async_symbol: false,
        static_member: modifiers.contains(ManagedModifier::Static),
        visibility,
    };
    let id = builder.emit_symbol(pending)?;
    capture_decorators(builder, node, &id)?;
    if let Some(type_node) = type_node {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node: type_node,
                owner: &id,
                kind: ReferenceKind::TypeOf,
                depth: 0,
            },
        )?;
    }
    let Some(body) = body else {
        return Ok(());
    };
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Property);
    builder.qualifiers.push(name);
    let result = builder.visit(body, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_variables(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: VariableVisit<'_>,
) -> Result<(), ExtractError> {
    let (declaration, type_node) = variable_declaration_and_type(input.node);
    let Some(declaration) = declaration else {
        return builder.visit_named_children(input.node, input.depth);
    };
    let modifiers = managed_modifiers(builder, input.node)?;
    let visibility = (input.kind == SymbolKind::Field)
        .then(|| managed_visibility(builder, modifiers, false))
        .flatten();
    for declarator in
        named_children(declaration).filter(|child| child.kind() == "variable_declarator")
    {
        builder.context.ensure_active()?;
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        let name = builder.context.owned_text(name_node)?;
        let pending = PendingSymbol {
            kind: input.kind,
            name: name.clone(),
            span_node: declarator,
            structural_node: declarator,
            doc_anchor: input.node,
            body_node: None,
            declaration_only: false,
            signature: (input.kind == SymbolKind::Field)
                .then(|| managed_typed_name(builder, type_node, &name))
                .transpose()?
                .flatten(),
            export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
            async_symbol: false,
            static_member: input.kind == SymbolKind::Field
                && (modifiers.contains(ManagedModifier::Static)
                    || modifiers.contains(ManagedModifier::Const)
                    || (builder.context.snapshot.language() == SourceLanguage::Java
                        && matches!(
                            builder.native_owner_kinds.last(),
                            Some(SymbolKind::Interface)
                        ))),
            visibility,
        };
        let id = builder.emit_symbol(pending)?;
        capture_decorators(builder, input.node, &id)?;
        if let Some(type_node) = type_node {
            capture_type_references(
                builder,
                TypeReferenceCapture {
                    node: type_node,
                    owner: &id,
                    kind: ReferenceKind::TypeOf,
                    depth: 0,
                },
            )?;
        }
        if let Some(value) = declarator
            .child_by_field_name("value")
            .or_else(|| named_children(declarator).find(|child| is_expression_kind(child.kind())))
        {
            builder.visit(value, input.depth.saturating_add(1))?;
        }
    }
    Ok(())
}

fn variable_declaration_and_type(node: Node<'_>) -> (Option<Node<'_>>, Option<Node<'_>>) {
    if matches!(
        node.kind(),
        "field_declaration" | "local_variable_declaration" | "constant_declaration"
    ) && node.child_by_field_name("declarator").is_some()
    {
        return (Some(node), node.child_by_field_name("type"));
    }
    let declaration = named_children(node).find(|child| child.kind() == "variable_declaration");
    let type_node = declaration.and_then(|declaration| declaration.child_by_field_name("type"));
    (declaration, type_node)
}

fn is_expression_kind(kind: &str) -> bool {
    kind.ends_with("_expression") || matches!(kind, "expression" | "array_initializer")
}

fn managed_typed_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    type_node: Option<Node<'_>>,
    name: &str,
) -> Result<Option<String>, ExtractError> {
    let Some(type_node) = type_node else {
        return Ok(None);
    };
    let type_text = builder.context.text(type_node).trim();
    let length = type_text
        .len()
        .checked_add(name.len())
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        return Ok(None);
    }
    builder.context.budget.ensure_string_length(length)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(type_text);
    signature.push(' ');
    signature.push_str(name);
    if !callable_signature_is_literal_free(&signature) {
        return Ok(None);
    }
    Ok(Some(signature))
}

fn visit_enum_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let pending = PendingSymbol {
        kind: SymbolKind::EnumMember,
        name: builder.context.owned_text(name_node)?,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: true,
        visibility: Some(Visibility::Public),
    };
    let id = builder.emit_symbol(pending)?;
    capture_decorators(builder, node, &id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
enum ManagedModifier {
    Public = 1 << 0,
    Private = 1 << 1,
    Protected = 1 << 2,
    Internal = 1 << 3,
    Static = 1 << 4,
    Async = 1 << 5,
    Const = 1 << 6,
    Decorated = 1 << 7,
}

#[derive(Clone, Copy, Debug, Default)]
struct ManagedModifiers(u16);

impl ManagedModifiers {
    const fn contains(self, modifier: ManagedModifier) -> bool {
        self.0 & modifier as u16 != 0
    }

    const fn insert(&mut self, modifier: ManagedModifier) {
        self.0 |= modifier as u16;
    }

    const fn explicit_visibility(self) -> Option<Visibility> {
        if self.contains(ManagedModifier::Public) {
            Some(Visibility::Public)
        } else if self.contains(ManagedModifier::Private) {
            Some(Visibility::Private)
        } else if self.contains(ManagedModifier::Protected) {
            Some(Visibility::Protected)
        } else if self.contains(ManagedModifier::Internal) {
            Some(Visibility::Internal)
        } else {
            None
        }
    }
}

fn managed_visibility(
    builder: &ExtractionBuilder<'_, '_>,
    modifiers: ManagedModifiers,
    type_declaration: bool,
) -> Option<Visibility> {
    if let Some(visibility) = modifiers.explicit_visibility() {
        return Some(visibility);
    }
    if matches!(
        builder.native_owner_kinds.last(),
        Some(SymbolKind::Interface)
    ) {
        return Some(Visibility::Public);
    }
    if builder.context.snapshot.language() == SourceLanguage::CSharp {
        if type_declaration
            && matches!(
                builder.native_owner_kinds.last(),
                None | Some(SymbolKind::Namespace)
            )
        {
            Some(Visibility::Internal)
        } else {
            Some(Visibility::Private)
        }
    } else {
        None
    }
}

fn managed_modifiers(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<ManagedModifiers, ExtractError> {
    let mut modifiers = ManagedModifiers::default();
    for child in children(node) {
        builder.context.ensure_active()?;
        capture_modifier(builder, child, &mut modifiers)?;
    }
    Ok(modifiers)
}

fn capture_modifier(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    modifiers: &mut ManagedModifiers,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    update_modifier(node.kind(), builder.context.text(node).trim(), modifiers);
    if matches!(node.kind(), "modifier" | "modifiers" | "attribute_list") {
        for child in children(node) {
            builder.context.ensure_active()?;
            update_modifier(child.kind(), builder.context.text(child).trim(), modifiers);
        }
    }
    Ok(())
}

/// Modifier keywords, matched case-insensitively because Apex keywords are
/// case-insensitive (`PUBLIC static`). Apex `global` widens `public` beyond the
/// namespace and is modelled as public.
const MODIFIER_KEYWORDS: &[(&str, ManagedModifier)] = &[
    ("public", ManagedModifier::Public),
    ("global", ManagedModifier::Public),
    ("private", ManagedModifier::Private),
    ("protected", ManagedModifier::Protected),
    ("internal", ManagedModifier::Internal),
    ("static", ManagedModifier::Static),
    ("async", ManagedModifier::Async),
    ("const", ManagedModifier::Const),
];

fn update_modifier(kind: &str, text: &str, modifiers: &mut ManagedModifiers) {
    let token = if matches!(kind, "modifier" | "modifiers") {
        text
    } else {
        kind
    };
    if let Some((_, modifier)) = MODIFIER_KEYWORDS
        .iter()
        .find(|(keyword, _)| token.eq_ignore_ascii_case(keyword))
    {
        modifiers.insert(*modifier);
    }
    if is_decorator(kind) {
        modifiers.insert(ManagedModifier::Decorated);
    }
}

fn capture_decorators(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for child in named_children(node) {
        builder.context.ensure_active()?;
        if is_decorator(child.kind()) {
            capture_decorator(builder, child, owner)?;
        } else if matches!(child.kind(), "modifiers" | "attribute_list") {
            for candidate in
                named_children(child).filter(|candidate| is_decorator(candidate.kind()))
            {
                builder.context.ensure_active()?;
                capture_decorator(builder, candidate, owner)?;
            }
        }
    }
    Ok(())
}

fn is_decorator(kind: &str) -> bool {
    matches!(kind, "annotation" | "marker_annotation" | "attribute")
}

fn capture_decorator(
    builder: &mut ExtractionBuilder<'_, '_>,
    decorator: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(name_node) = decorator.child_by_field_name("name") else {
        return Ok(());
    };
    let name = safe_reference_node_text(builder, name_node)?;
    let Some(name) = name else {
        return Ok(());
    };
    push_named_reference(
        builder,
        PendingReference {
            owner: Some(owner.clone()),
            name,
            kind: ReferenceKind::Decorates,
            node: name_node,
        },
    )
}

fn capture_type_references(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Java | SourceLanguage::Apex => capture_java_type(builder, input),
        SourceLanguage::CSharp => capture_csharp_type(builder, input),
        _ => Ok(()),
    }
}

fn capture_java_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    if input.depth > MAX_TYPE_DEPTH {
        return Err(ExtractError::NestingLimit);
    }
    builder.context.ensure_active()?;
    match input.node.kind() {
        "scoped_type_identifier" => {
            push_managed_type_name(builder, input)?;
            capture_java_type_children(builder, input)?;
        }
        "type_identifier" => push_managed_type_name(builder, input)?,
        _ => capture_java_type_children(builder, input)?,
    }
    Ok(())
}

/// Visit only the direct children that contribute names to this Java type.
fn capture_java_type_children(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    for child in named_children(input.node).filter(|child| is_java_type_child(input, *child)) {
        capture_java_type(
            builder,
            TypeReferenceCapture {
                node: child,
                depth: input.depth.saturating_add(1),
                ..input
            },
        )?;
    }
    Ok(())
}

fn is_java_type_child(input: TypeReferenceCapture<'_, '_>, child: Node<'_>) -> bool {
    match input.node.kind() {
        "scoped_type_identifier" => {
            child.kind() == "type_arguments" && names_type_arguments(input.kind)
        }
        "generic_type" => {
            matches!(child.kind(), "type_identifier" | "scoped_type_identifier")
                || (child.kind() == "type_arguments" && names_type_arguments(input.kind))
        }
        _ => true,
    }
}

/// Whether a type reference of this kind also names its generic arguments.
///
/// `class Foo extends Bar<Baz>` inherits from `Bar` only; `Baz` is a type
/// argument, not a supertype. Usage kinds (`TypeOf`, `Returns`) keep naming the
/// arguments because `List<Account>` does depend on `Account`.
const fn names_type_arguments(kind: ReferenceKind) -> bool {
    !matches!(
        kind,
        ReferenceKind::Extends | ReferenceKind::Implements | ReferenceKind::Inherits
    )
}

fn capture_csharp_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    if input.depth > MAX_TYPE_DEPTH {
        return Err(ExtractError::NestingLimit);
    }
    builder.context.ensure_active()?;
    match input.node.kind() {
        "qualified_name" | "alias_qualified_name" => capture_csharp_qualified(builder, input)?,
        "generic_name" => capture_csharp_generic(builder, input)?,
        "identifier" => push_managed_type_name(builder, input)?,
        "predefined_type" | "implicit_type" => {}
        "tuple_element" => capture_csharp_tuple_element(builder, input)?,
        _ => capture_csharp_children(builder, input)?,
    }
    Ok(())
}

/// A tuple element contributes only its type; `(Item first, int count)`
/// names `Item`, never the element names `first`/`count`.
fn capture_csharp_tuple_element(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let Some(element_type) = input.node.child_by_field_name("type") else {
        return Ok(());
    };
    capture_csharp_type(
        builder,
        TypeReferenceCapture {
            node: element_type,
            depth: input.depth.saturating_add(1),
            ..input
        },
    )
}

fn capture_csharp_qualified(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    push_managed_type_name(builder, input)?;
    for child in
        descendants_including_root(input.node).filter(|child| child.kind() == "type_argument_list")
    {
        capture_csharp_arguments(builder, input, child)?;
    }
    Ok(())
}

fn capture_csharp_generic(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    for child in named_children(input.node) {
        match child.kind() {
            "identifier" => push_managed_type_name(
                builder,
                TypeReferenceCapture {
                    node: child,
                    ..input
                },
            )?,
            "type_argument_list" => capture_csharp_arguments(builder, input, child)?,
            _ => {}
        }
    }
    Ok(())
}

fn capture_csharp_arguments(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
    arguments: Node<'_>,
) -> Result<(), ExtractError> {
    for argument in named_children(arguments) {
        capture_csharp_type(
            builder,
            TypeReferenceCapture {
                node: argument,
                depth: input.depth.saturating_add(1),
                ..input
            },
        )?;
    }
    Ok(())
}

fn capture_csharp_children(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    for child in named_children(input.node) {
        capture_csharp_type(
            builder,
            TypeReferenceCapture {
                node: child,
                depth: input.depth.saturating_add(1),
                ..input
            },
        )?;
    }
    Ok(())
}

/// Emit a named Java or C# type through the shared managed-type normalization.
fn push_managed_type_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeReferenceCapture<'_, '_>,
) -> Result<(), ExtractError> {
    if let Some(name) = managed_outer_type_name(builder, input.node)? {
        push_named_reference(
            builder,
            PendingReference {
                owner: Some(input.owner.clone()),
                name,
                kind: input.kind,
                node: input.node,
            },
        )?;
    }
    Ok(())
}

pub(super) fn managed_outer_type_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let raw = builder.context.text(node).trim();
    let without_global = raw.strip_prefix("global::").unwrap_or(raw);
    // `Outer<A>.Inner<B>` names `Outer.Inner`: generic argument ranges are
    // removed rather than truncating the qualified chain at the first `<`.
    let nested = without_global
        .find('<')
        .is_some_and(|index| without_global[index..].contains('.'));
    if nested && without_global.len() > MAX_NESTED_GENERIC_BYTES {
        // Truncating would name a different (outer) type; abstain instead.
        return Ok(None);
    }
    let stripped;
    let head = if nested {
        stripped = strip_generic_arguments(without_global)?;
        stripped.as_str()
    } else {
        without_global
            .find('<')
            .and_then(|index| without_global.get(..index))
            .unwrap_or(without_global)
    };
    let outer = head.trim().trim_end_matches(['?', '*', '&']);
    if outer.is_empty()
        || outer.len() > MAX_REFERENCE_TARGET_BYTES
        || is_managed_builtin(outer)
        || !outer
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'$'))
    {
        return Ok(None);
    }
    builder.context.copy_text(outer).map(Some)
}

const MANAGED_BUILTIN_TYPES: &[&str] = &[
    "bool", "byte", "char", "decimal", "double", "dynamic", "float", "int", "long", "nint",
    "nuint", "object", "sbyte", "short", "string", "uint", "ulong", "ushort", "var", "void",
    "boolean",
];

fn is_managed_builtin(name: &str) -> bool {
    MANAGED_BUILTIN_TYPES.contains(&name)
}

fn capture_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let target = if node.kind() == "method_invocation" {
        let Some(name_node) = node.child_by_field_name("name") else {
            return Ok(());
        };
        java_call_name(builder, node.child_by_field_name("object"), name_node)?
    } else {
        let Some(function) = node.child_by_field_name("function") else {
            return Ok(());
        };
        safe_reference_node_text(builder, function)?
    };
    let Some(name) = target else {
        return Ok(());
    };
    super::jvm_type_lookup::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::Calls,
            node,
        },
    )
}

fn java_call_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    object: Option<Node<'_>>,
    name: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(local_name) = safe_reference_node_text(builder, name)? else {
        return Ok(None);
    };
    let Some(object) = object else {
        return Ok(Some(local_name));
    };
    let Some(object_name) = safe_reference_node_text(builder, object)? else {
        return Ok(Some(local_name));
    };
    let object_name = object_name.strip_prefix("this.").unwrap_or(&object_name);
    let length = object_name
        .len()
        .checked_add(local_name.len())
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_REFERENCE_TARGET_BYTES {
        return Ok(Some(local_name));
    }
    builder.context.budget.ensure_string_length(length)?;
    let mut qualified = String::new();
    qualified
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    qualified.push_str(object_name);
    qualified.push('.');
    qualified.push_str(&local_name);
    Ok(Some(qualified))
}

fn safe_reference_node_text(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    for descendant in descendants_including_root(node) {
        builder.context.ensure_active()?;
        if is_literal_kind(descendant.kind()) {
            return Ok(None);
        }
    }
    let raw = builder.context.text(node).trim();
    if raw.len() > MAX_REFERENCE_TARGET_BYTES {
        return Ok(None);
    }
    let normalized = strip_generic_arguments(raw)?;
    if normalized.is_empty()
        || normalized.len() > MAX_REFERENCE_TARGET_BYTES
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'$'))
    {
        return Ok(None);
    }
    builder.context.copy_text(&normalized).map(Some)
}

fn strip_generic_arguments(raw: &str) -> Result<String, ExtractError> {
    let mut normalized = String::new();
    normalized
        .try_reserve(raw.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut generic_depth = 0_usize;
    for character in raw.chars() {
        match character {
            '<' => generic_depth = generic_depth.saturating_add(1),
            '>' => generic_depth = generic_depth.saturating_sub(1),
            _ if generic_depth == 0 && !character.is_whitespace() => normalized.push(character),
            _ => {}
        }
    }
    Ok(normalized)
}

fn is_literal_kind(kind: &str) -> bool {
    kind.contains("string")
        || kind.contains("character")
        || kind.contains("integer_literal")
        || kind.contains("real_literal")
        || kind.contains("number_literal")
}

fn capture_member_access(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if is_csharp_invocation_target(node) {
        return Ok(());
    }
    let field = if node.kind() == "field_access" {
        node.child_by_field_name("field")
    } else {
        node.child_by_field_name("name")
    };
    let Some(field) = field else {
        return Ok(());
    };
    let Some(name) = safe_reference_node_text(builder, field)? else {
        return Ok(());
    };
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::FieldAccess,
            node: field,
        },
    )
}

fn is_csharp_invocation_target(node: Node<'_>) -> bool {
    let Some(parent) = node
        .parent()
        .filter(|parent| parent.kind() == "invocation_expression")
    else {
        return false;
    };
    parent
        .child_by_field_name("function")
        .is_some_and(|function| {
            function.start_byte() == node.start_byte() && function.end_byte() == node.end_byte()
        })
}

fn push_named_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    reference: PendingReference<'_>,
) -> Result<(), ExtractError> {
    super::jvm_type_lookup::push_reference(builder, reference)
}
