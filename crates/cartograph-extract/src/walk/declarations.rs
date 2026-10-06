use cartograph_domain::{ReferenceKind, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, module_system, references,
    syntax::{descendants, export_flags, named_children, span_for, visibility},
    type_contracts,
};

const MAX_TYPE_ALIAS_SIGNATURE_BYTES: usize = 64 * 1024;

pub(super) fn visit_export(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let source_node = node.child_by_field_name("source");
    if source_node
        .is_some_and(|source| module_system::screened_import_source(builder, source).is_none())
    {
        return Ok(());
    }
    let source = source_node
        .map(|source| builder.context.owned_unquoted_text(source))
        .transpose()?;
    let clause = named_children(node).find(|child| child.kind() == "export_clause");
    if let Some(clause) = clause {
        emit_export_clause(builder, clause, source.as_deref())?;
    } else if let (Some(module_name), Some(source_node)) = (source, source_node) {
        emit_export_all(
            builder,
            ExportAllInput {
                node,
                source_node,
                module_name,
            },
        )?;
    }
    builder.visit_named_children(node, depth)
}

fn emit_export_clause(
    builder: &mut ExtractionBuilder<'_, '_>,
    clause: Node<'_>,
    source: Option<&str>,
) -> Result<(), ExtractError> {
    for specifier in named_children(clause) {
        builder.context.ensure_active()?;
        if specifier.kind() != "export_specifier" {
            continue;
        }
        let Some(name_node) = specifier.child_by_field_name("name") else {
            continue;
        };
        let local_name = builder.context.owned_unquoted_text(name_node)?;
        let public_name = match specifier.child_by_field_name("alias") {
            Some(alias) => builder.context.owned_unquoted_text(alias)?,
            None => local_name.clone(),
        };
        if source.is_some() || public_name != local_name {
            module_system::emit_export_alias(
                builder,
                module_system::ExportAlias {
                    public_name,
                    local_name,
                    span_node: specifier,
                    reference_node: name_node,
                    source: source.map(str::to_owned),
                },
            )?;
        }
    }
    Ok(())
}

struct ExportAllInput<'tree> {
    node: Node<'tree>,
    source_node: Node<'tree>,
    module_name: String,
}

fn emit_export_all(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ExportAllInput<'_>,
) -> Result<(), ExtractError> {
    let ExportAllInput {
        node,
        source_node,
        module_name,
    } = input;
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Import,
        name: module_name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: module_name.clone(),
            kind: ReferenceKind::Imports,
            node,
        },
    )?;
    // The pinned grammar recovers unsupported `export type *` as an ordinary
    // wildcard with an error child. Preserve that uncertainty in the binding
    // rather than exposing runtime values through recovery syntax.
    if node.has_error() {
        return builder.emit_import_binding(ExtractedImportBinding {
            kind: ImportBindingKind::ReExportUncertain,
            module_specifier: module_name,
            imported_name: "*".to_owned(),
            local_name: "*".to_owned(),
            span: span_for(source_node)?,
        });
    }
    let namespace = named_children(node).find(|child| child.kind() == "namespace_export");
    if let Some((namespace, public_node)) =
        namespace.and_then(|namespace| namespace.named_child(0).map(|node| (namespace, node)))
    {
        return module_system::emit_namespace_reexport(
            builder,
            module_system::NamespaceReexportInput::new(namespace, public_node, module_name),
        );
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::ReExportAll,
        module_specifier: module_name,
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: span_for(source_node)?,
    })
}

pub(super) fn visit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(source_node) = node
        .child_by_field_name("source")
        .and_then(|source| module_system::screened_import_source(builder, source))
    else {
        return Ok(());
    };
    let module_name = builder.context.owned_unquoted_text(source_node)?;
    emit_import_declaration(builder, node, &module_name)?;

    if let Some(clause) = named_children(node).find(|child| child.kind() == "import_clause") {
        emit_import_clause_bindings(builder, clause, &module_name)?;
    }
    emit_named_import_bindings(builder, node, &module_name)
}

fn emit_import_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    module_name: &str,
) -> Result<(), ExtractError> {
    let pending = PendingSymbol {
        kind: SymbolKind::Import,
        name: module_name.to_owned(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: Some(builder.context.owned_text(node)?),
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol(pending)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: module_name.to_owned(),
            kind: ReferenceKind::Imports,
            node,
        },
    )
}

fn emit_import_clause_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    clause: Node<'_>,
    module_name: &str,
) -> Result<(), ExtractError> {
    for child in named_children(clause) {
        builder.context.ensure_active()?;
        match child.kind() {
            "identifier" => {
                let local_name = builder.context.owned_text(child)?;
                builder.emit_import_binding(ExtractedImportBinding {
                    kind: ImportBindingKind::Default,
                    module_specifier: module_name.to_owned(),
                    imported_name: "default".to_owned(),
                    local_name,
                    span: span_for(child)?,
                })?;
            }
            "namespace_import" => {
                let Some(local) = child.named_child(0) else {
                    continue;
                };
                let local_name = builder.context.owned_text(local)?;
                builder.emit_import_binding(ExtractedImportBinding {
                    kind: ImportBindingKind::Namespace,
                    module_specifier: module_name.to_owned(),
                    imported_name: "*".to_owned(),
                    local_name,
                    span: span_for(local)?,
                })?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn emit_named_import_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    module_name: &str,
) -> Result<(), ExtractError> {
    for specifier in descendants(node) {
        builder.context.ensure_active()?;
        if specifier.kind() != "import_specifier" {
            continue;
        }
        if let Some(name_node) = specifier.child_by_field_name("name") {
            let name = builder.context.owned_unquoted_text(name_node)?;
            if !module_system::import_literal_is_safe(&name) {
                continue;
            }
            let local_node = specifier.child_by_field_name("alias").unwrap_or(name_node);
            if local_node.kind() == "identifier" {
                let local_name = builder.context.owned_text(local_node)?;
                builder.emit_import_binding(ExtractedImportBinding {
                    kind: ImportBindingKind::Named,
                    module_specifier: module_name.to_owned(),
                    imported_name: name.clone(),
                    local_name,
                    span: span_for(name_node)?,
                })?;
            }
            references::push_reference(
                builder,
                PendingReference {
                    owner: None,
                    name,
                    kind: ReferenceKind::References,
                    node: name_node,
                },
            )?;
        }
    }
    Ok(())
}

pub(super) fn visit_type_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let (exported, default_export) = export_flags(node);
    let signature = (node.end_byte().saturating_sub(node.start_byte())
        <= MAX_TYPE_ALIAS_SIGNATURE_BYTES)
        .then(|| builder.context.owned_text(node))
        .transpose()?;
    let name = builder.context.owned_text(name_node)?;
    let pending = PendingSymbol {
        kind: SymbolKind::TypeAlias,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature,
        export: crate::SymbolExportFlags::new(exported, default_export),
        async_symbol: false,
        static_member: false,
        visibility: visibility(node, builder.context.source()),
    };
    let id = builder.emit_symbol(pending)?;
    let alias_exported = builder
        .facts
        .symbols
        .last()
        .is_some_and(|symbol| symbol.export.exported);
    if let Some(value) = node.child_by_field_name("value") {
        references::capture_type_nodes(builder, value, &id)?;
        builder.javascript.whole_type_owners.insert(id.clone());
        type_contracts::emit_contract_properties(
            builder,
            value,
            type_contracts::ContractAlias::new(
                &id,
                &name,
                crate::SymbolExportFlags::named(alias_exported),
            ),
        )?;
        builder.owners.push(id);
        builder.visit(value, depth.saturating_add(1))?;
        builder.owners.pop();
    }
    Ok(())
}

pub(super) fn visit_enum(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let (exported, default_export) = export_flags(node);
    let pending = PendingSymbol {
        kind: SymbolKind::Enum,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(exported, default_export),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    let id = builder.emit_symbol(pending)?;
    builder.owners.push(id);
    builder.qualifiers.push(name);
    if let Some(body) = node.child_by_field_name("body") {
        for child in named_children(body) {
            builder.context.ensure_active()?;
            let member_name = match child.kind() {
                "enum_assignment" => child
                    .child_by_field_name("name")
                    .or_else(|| child.named_child(0)),
                "property_identifier"
                | "private_property_identifier"
                | "string"
                | "number"
                | "computed_property_name" => Some(child),
                _ => None,
            };
            let Some(member_name) = member_name else {
                continue;
            };
            let pending = PendingSymbol {
                kind: SymbolKind::EnumMember,
                name: builder.context.owned_text(member_name)?,
                span_node: child,
                structural_node: child,
                doc_anchor: child,
                body_node: None,
                declaration_only: false,
                signature: None,
                export: crate::SymbolExportFlags::new(false, false),
                async_symbol: false,
                static_member: false,
                visibility: None,
            };
            builder.emit_symbol(pending)?;
        }
    }
    builder.qualifiers.pop();
    builder.owners.pop();
    Ok(())
}
