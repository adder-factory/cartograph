use std::collections::BTreeSet;

use cartograph_domain::{
    ReferenceKind, SymbolId, SymbolKind, callable_signature_is_literal_free,
    declaration_value_is_search_safe,
};
use tree_sitter::Node;

use crate::{
    ExtractError, ExtractedReference,
    walk::{
        AstVisitBudget, ExtractionBuilder, PendingSymbol,
        syntax::{named_children, span_for},
    },
};

const MAX_SCAN_DEPTH: usize = 128;
const MAX_RELATIONS_PER_DECLARATION: usize = 4_096;

#[derive(Default)]
struct ScanBudget {
    visits: AstVisitBudget<MAX_SCAN_DEPTH>,
}

struct RelationScanInput<'tree, 'scan> {
    node: Node<'tree>,
    owner: &'scan SymbolId,
    depth: usize,
    budget: &'scan mut ScanBudget,
    seen: &'scan mut BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum RelationScanKind {
    ForeignKeys,
    Query,
}

#[derive(Clone, Copy)]
struct RelationEmission<'tree, 'owner> {
    root: Node<'tree>,
    owner: &'owner SymbolId,
    kind: RelationScanKind,
}

impl<'tree, 'owner> RelationEmission<'tree, 'owner> {
    const fn foreign_keys(root: Node<'tree>, owner: &'owner SymbolId) -> Self {
        Self {
            root,
            owner,
            kind: RelationScanKind::ForeignKeys,
        }
    }

    const fn query(root: Node<'tree>, owner: &'owner SymbolId) -> Self {
        Self {
            root,
            owner,
            kind: RelationScanKind::Query,
        }
    }
}

#[derive(Clone, Copy)]
struct LabeledSignatureInput<'text> {
    label: &'text str,
    name: &'text str,
    suffix: Option<&'text str>,
}

struct RelationInput<'tree, 'reference> {
    owner: &'reference SymbolId,
    node: Node<'tree>,
    name: String,
    kind: ReferenceKind,
}

#[derive(Clone, Copy)]
struct OwnerScopeInput<'scope> {
    owner: &'scope SymbolId,
    kind: SymbolKind,
    name: &'scope str,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    _depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "statement" => {
            if let Some(statement) = named_children(node).next() {
                emit_statement(builder, statement)?;
            }
            Ok(true)
        }
        "ERROR" => {
            recover_create_function(builder, node)?;
            Ok(true)
        }
        kind if is_create_declaration(kind) => {
            emit_statement(builder, node)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn is_create_declaration(kind: &str) -> bool {
    matches!(
        kind,
        "create_table"
            | "create_view"
            | "create_function"
            | "create_trigger"
            | "create_type"
            | "create_schema"
    )
}

fn emit_statement(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "create_table" => emit_table(builder, node, false),
        "create_view" => emit_table(builder, node, true),
        "create_function" => emit_function(builder, node),
        "create_trigger" => emit_trigger(builder, node),
        "create_type" => emit_type(builder, node),
        "create_schema" => emit_schema(builder, node),
        _ => Ok(()),
    }
}

fn emit_table(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    view: bool,
) -> Result<(), ExtractError> {
    let Some(reference) = direct_child(node, "object_reference") else {
        return Ok(());
    };
    let Some(name) = qualified_name(builder, reference)? else {
        return Ok(());
    };
    let label = if view { "CREATE VIEW" } else { "CREATE TABLE" };
    let signature = labeled_signature(
        builder,
        LabeledSignatureInput {
            label,
            name: &name,
            suffix: None,
        },
    )?;
    let owner = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Table,
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
    if view {
        if let Some(query) = direct_child(node, "create_query") {
            emit_relations(builder, RelationEmission::query(query, &owner))?;
        }
        return Ok(());
    }
    let Some(columns) = direct_child(node, "column_definitions") else {
        return Ok(());
    };
    with_owner(
        builder,
        OwnerScopeInput {
            owner: &owner,
            kind: SymbolKind::Table,
            name: &name,
        },
        |builder| emit_columns(builder, columns),
    )?;
    emit_relations(builder, RelationEmission::foreign_keys(columns, &owner))
}

fn emit_columns(
    builder: &mut ExtractionBuilder<'_, '_>,
    columns: Node<'_>,
) -> Result<(), ExtractError> {
    for column in named_children(columns) {
        builder.context.ensure_active()?;
        if column.kind() != "column_definition" {
            continue;
        }
        let Some(name_node) = column.child_by_field_name("name") else {
            continue;
        };
        let name = normalized_identifier(builder, name_node)?;
        let type_node = column
            .child_by_field_name("type")
            .or_else(|| column.child_by_field_name("custom_type"));
        let signature = type_node
            .map(|node| safe_type_signature(builder, node))
            .transpose()?;
        let field = builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Field,
            name,
            span_node: column,
            structural_node: column,
            doc_anchor: column,
            body_node: None,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })?;
        if let Some(custom_type) = column.child_by_field_name("custom_type")
            && let Some(target) = qualified_name(builder, custom_type)?
        {
            builder.emit_reference(ExtractedReference {
                owner: Some(field),
                name: target,
                resolution_name: None,
                kind: ReferenceKind::TypeOf,
                span: span_for(custom_type)?,
            })?;
        }
    }
    Ok(())
}

fn emit_relations(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: RelationEmission<'_, '_>,
) -> Result<(), ExtractError> {
    let mut budget = ScanBudget::default();
    let mut seen = BTreeSet::new();
    let scan = RelationScanInput {
        node: input.root,
        owner: input.owner,
        depth: 0,
        budget: &mut budget,
        seen: &mut seen,
    };
    match input.kind {
        RelationScanKind::ForeignKeys => scan_foreign_keys(builder, scan),
        RelationScanKind::Query => scan_query_relations(builder, scan),
    }
}

fn scan_foreign_keys(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: RelationScanInput<'_, '_>,
) -> Result<(), ExtractError> {
    let RelationScanInput {
        node,
        owner,
        depth,
        budget,
        seen,
    } = input;
    budget.visits.observe(builder, depth)?;
    let mut references_next = false;
    for child in named_children(node) {
        if child.kind() == "keyword_references" {
            references_next = true;
            continue;
        }
        if references_next && child.kind() == "object_reference" {
            if let Some(name) = qualified_name(builder, child)?
                && seen.insert(name.clone())
            {
                if seen.len() > MAX_RELATIONS_PER_DECLARATION {
                    return Err(ExtractError::OutputLimit);
                }
                emit_relation(
                    builder,
                    RelationInput {
                        owner,
                        node: child,
                        name,
                        kind: ReferenceKind::References,
                    },
                )?;
            }
            references_next = false;
            continue;
        }
        scan_foreign_keys(
            builder,
            RelationScanInput {
                node: child,
                owner,
                depth: depth.saturating_add(1),
                budget: &mut *budget,
                seen: &mut *seen,
            },
        )?;
    }
    Ok(())
}

fn scan_query_relations(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: RelationScanInput<'_, '_>,
) -> Result<(), ExtractError> {
    let RelationScanInput {
        node,
        owner,
        depth,
        budget,
        seen,
    } = input;
    budget.visits.observe(builder, depth)?;
    if node.kind() == "relation"
        && let Some(reference) = direct_child(node, "object_reference")
        && let Some(name) = qualified_name(builder, reference)?
    {
        let identity = name.to_ascii_lowercase();
        if seen.insert(identity) {
            if seen.len() > MAX_RELATIONS_PER_DECLARATION {
                return Err(ExtractError::OutputLimit);
            }
            emit_relation(
                builder,
                RelationInput {
                    owner,
                    node: reference,
                    name,
                    kind: ReferenceKind::References,
                },
            )?;
        }
        return Ok(());
    }
    for child in named_children(node) {
        scan_query_relations(
            builder,
            RelationScanInput {
                node: child,
                owner,
                depth: depth.saturating_add(1),
                budget: &mut *budget,
                seen: &mut *seen,
            },
        )?;
    }
    Ok(())
}

fn emit_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(reference) = direct_child(node, "object_reference") else {
        return Ok(());
    };
    let Some(name) = qualified_name(builder, reference)? else {
        return Ok(());
    };
    let arguments = direct_child(node, "function_arguments")
        .map(|arguments| builder.context.text(arguments).trim())
        .filter(|arguments| callable_signature_is_literal_free(arguments));
    let signature = labeled_signature(
        builder,
        LabeledSignatureInput {
            label: "CREATE FUNCTION",
            name: &name,
            suffix: arguments,
        },
    )?;
    let owner = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Function,
        name,
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
    if let Some(body) = direct_child(node, "function_body") {
        emit_relations(builder, RelationEmission::query(body, &owner))?;
    }
    Ok(())
}

fn emit_trigger(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let mut object_references =
        named_children(node).filter(|child| child.kind() == "object_reference");
    let Some(name_node) = object_references.next() else {
        return Ok(());
    };
    let Some(name) = qualified_name(builder, name_node)? else {
        return Ok(());
    };
    let signature = labeled_signature(
        builder,
        LabeledSignatureInput {
            label: "CREATE TRIGGER",
            name: &name,
            suffix: None,
        },
    )?;
    let owner = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Function,
        name,
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
    if let Some(target) = object_reference_after(node, "keyword_on")
        && let Some(target_name) = qualified_name(builder, target)?
    {
        emit_relation(
            builder,
            RelationInput {
                owner: &owner,
                node: target,
                name: target_name,
                kind: ReferenceKind::References,
            },
        )?;
    }
    if let Some(function) = object_reference_after(node, "keyword_execute")
        && let Some(function_name) = qualified_name(builder, function)?
    {
        emit_relation(
            builder,
            RelationInput {
                owner: &owner,
                node: function,
                name: function_name,
                kind: ReferenceKind::Calls,
            },
        )?;
    }
    Ok(())
}

fn emit_type(builder: &mut ExtractionBuilder<'_, '_>, node: Node<'_>) -> Result<(), ExtractError> {
    let Some(reference) = direct_child(node, "object_reference") else {
        return Ok(());
    };
    let Some(name) = qualified_name(builder, reference)? else {
        return Ok(());
    };
    let enum_type = direct_child(node, "keyword_enum").is_some();
    let kind = if enum_type {
        SymbolKind::Enum
    } else {
        SymbolKind::TypeAlias
    };
    let signature = labeled_signature(
        builder,
        LabeledSignatureInput {
            label: "CREATE TYPE",
            name: &name,
            suffix: None,
        },
    )?;
    let owner = builder.emit_symbol(PendingSymbol {
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
    })?;
    if enum_type {
        if let Some(elements) = direct_child(node, "enum_elements") {
            with_owner(
                builder,
                OwnerScopeInput {
                    owner: &owner,
                    kind,
                    name: &name,
                },
                |builder| emit_enum_members(builder, elements),
            )?;
        }
    } else if let Some(columns) = direct_child(node, "column_definitions") {
        with_owner(
            builder,
            OwnerScopeInput {
                owner: &owner,
                kind,
                name: &name,
            },
            |builder| emit_columns(builder, columns),
        )?;
    }
    Ok(())
}

fn emit_enum_members(
    builder: &mut ExtractionBuilder<'_, '_>,
    elements: Node<'_>,
) -> Result<(), ExtractError> {
    let mut seen = BTreeSet::new();
    for element in named_children(elements) {
        builder.context.ensure_active()?;
        if element.kind() != "literal" {
            continue;
        }
        let value = builder.context.owned_unquoted_text(element)?;
        if value.is_empty()
            || !declaration_value_is_search_safe(&value)
            || !seen.insert(value.clone())
        {
            continue;
        }
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::EnumMember,
            name: value.clone(),
            span_node: element,
            structural_node: element,
            doc_anchor: element,
            body_node: None,
            declaration_only: false,
            signature: Some(value),
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })?;
    }
    Ok(())
}

fn emit_schema(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(identifier) = direct_child(node, "identifier") else {
        return Ok(());
    };
    let name = normalized_identifier(builder, identifier)?;
    let signature = labeled_signature(
        builder,
        LabeledSignatureInput {
            label: "CREATE SCHEMA",
            name: &name,
            suffix: None,
        },
    )?;
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Namespace,
        name,
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
    Ok(())
}

fn recover_create_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let kinds = named_children(node)
        .map(|child| child.kind())
        .collect::<BTreeSet<_>>();
    if kinds.contains("keyword_create")
        && kinds.contains("keyword_function")
        && kinds.contains("object_reference")
    {
        emit_function(builder, node)?;
    }
    Ok(())
}

fn object_reference_after<'tree>(node: Node<'tree>, marker: &str) -> Option<Node<'tree>> {
    let mut seen = false;
    for child in named_children(node) {
        if child.kind() == marker {
            seen = true;
        } else if seen && child.kind() == "object_reference" {
            return Some(child);
        }
    }
    None
}

fn qualified_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let identifiers = named_children(node)
        .filter(|child| child.kind() == "identifier")
        .collect::<Vec<_>>();
    if identifiers.is_empty() {
        let value = builder.context.owned_text(node)?;
        let normalized = strip_identifier_quotes(value.trim());
        return if normalized.is_empty() {
            Ok(None)
        } else {
            builder.context.copy_text(normalized).map(Some)
        };
    }
    let mut parts = Vec::new();
    parts
        .try_reserve(identifiers.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    for identifier in identifiers {
        parts.push(normalized_identifier(builder, identifier)?);
    }
    let length = parts
        .iter()
        .try_fold(parts.len().saturating_sub(1), |length, part| {
            length.checked_add(part.len())
        });
    let mut qualified = String::new();
    qualified
        .try_reserve(length.ok_or(ExtractError::OutputLimit)?)
        .map_err(|_| ExtractError::OutputLimit)?;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            qualified.push('.');
        }
        qualified.push_str(part);
    }
    builder.context.copy_text(&qualified).map(Some)
}

fn normalized_identifier(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<String, ExtractError> {
    let raw = builder.context.text(node).trim();
    builder.context.copy_text(strip_identifier_quotes(raw))
}

fn strip_identifier_quotes(value: &str) -> &str {
    value
        .strip_circumfix('"', '"')
        .or_else(|| value.strip_circumfix('`', '`'))
        .or_else(|| value.strip_circumfix('[', ']'))
        .unwrap_or(value)
}

fn safe_type_signature(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<String, ExtractError> {
    let raw = builder.context.text(node).trim();
    if callable_signature_is_literal_free(raw) {
        builder.context.copy_text(raw)
    } else if let Some(identifier) = descendant_identifier(node, 0) {
        builder.context.copy_text(strip_identifier_quotes(
            builder.context.text(identifier).trim(),
        ))
    } else {
        Ok(String::new())
    }
}

fn descendant_identifier(node: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > MAX_SCAN_DEPTH {
        return None;
    }
    if node.kind() == "identifier" {
        return Some(node);
    }
    named_children(node).find_map(|child| descendant_identifier(child, depth.saturating_add(1)))
}

fn labeled_signature(
    builder: &ExtractionBuilder<'_, '_>,
    input: LabeledSignatureInput<'_>,
) -> Result<String, ExtractError> {
    let suffix = input.suffix.unwrap_or_default();
    let length = input
        .label
        .len()
        .checked_add(input.name.len())
        .and_then(|length| length.checked_add(suffix.len()))
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(input.label);
    signature.push(' ');
    signature.push_str(input.name);
    signature.push_str(suffix);
    builder.context.copy_text(&signature)
}

fn emit_relation(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: RelationInput<'_, '_>,
) -> Result<(), ExtractError> {
    builder.emit_reference(ExtractedReference {
        owner: Some(input.owner.clone()),
        name: input.name,
        resolution_name: None,
        kind: input.kind,
        span: span_for(input.node)?,
    })
}

fn with_owner<T>(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: OwnerScopeInput<'_>,
    action: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<T, ExtractError>,
) -> Result<T, ExtractError> {
    builder.owners.push(input.owner.clone());
    builder.native_owner_kinds.push(input.kind);
    builder.native_visibilities.push(None);
    builder
        .qualifiers
        .push(builder.context.copy_text(input.name)?);
    let result = action(builder);
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn direct_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    named_children(node).find(|child| child.kind() == kind)
}
