//! Ada and VHDL structural extraction.
//!
//! Both languages use case-insensitive basic identifiers and compilation units
//! whose source file name is not always the unit name.  The family therefore
//! emits canonical lower-case names, explicit unit bindings, and typed calls
//! instead of relying on generic filename or identifier heuristics.

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{
    ExtractError, ExtractedImportBinding, ExtractedReference, ImportBindingKind, SymbolExportFlags,
};

use super::{
    ExtractionBuilder, PendingSymbol,
    syntax::{named_children, span_for},
    with_root_scope,
};

const MAXIMUM_UNIT_NAME_BYTES: usize = 512;

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Ada => visit_ada_declaration(builder, node, depth),
        SourceLanguage::Vhdl => visit_vhdl_declaration(builder, node, depth),
        _ => Ok(false),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Ada => capture_ada_usage(builder, node),
        SourceLanguage::Vhdl => capture_vhdl_usage(builder, node),
        _ => Ok(()),
    }
}

fn visit_ada_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "with_clause" => visit_ada_context_clause(builder, node, false)?,
        "use_clause" => visit_ada_context_clause(builder, node, true)?,
        "package_declaration" => {
            visit_container(
                builder,
                ContainerVisit {
                    node,
                    depth,
                    name: node.child_by_field_name("name"),
                    kind: SymbolKind::Module,
                    declaration_only: true,
                },
            )?;
        }
        "package_body" => {
            visit_container(
                builder,
                ContainerVisit {
                    node,
                    depth,
                    name: node.child_by_field_name("name"),
                    kind: SymbolKind::Module,
                    declaration_only: false,
                },
            )?;
        }
        "subprogram_body"
        | "subprogram_declaration"
        | "expression_function_declaration"
        | "null_procedure_declaration" => visit_ada_callable(builder, node, depth)?,
        "full_type_declaration"
        | "private_type_declaration"
        | "incomplete_type_declaration"
        | "subtype_declaration" => {
            visit_type_declaration(builder, node, depth)?;
        }
        "object_declaration" => visit_ada_objects(builder, node)?,
        "component_declaration" => visit_first_named_binding(builder, node, SymbolKind::Field)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_vhdl_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "library_clause" => visit_vhdl_library_clause(builder, node)?,
        "use_clause" => visit_vhdl_use_clause(builder, node)?,
        "entity_declaration" | "component_declaration" => {
            visit_container(
                builder,
                ContainerVisit {
                    node,
                    depth,
                    name: node.child_by_field_name("name"),
                    kind: SymbolKind::Interface,
                    declaration_only: true,
                },
            )?;
        }
        "architecture_body" => visit_vhdl_architecture(builder, node, depth)?,
        "package_declaration" => {
            visit_container(
                builder,
                ContainerVisit {
                    node,
                    depth,
                    name: node.child_by_field_name("name"),
                    kind: SymbolKind::Module,
                    declaration_only: true,
                },
            )?;
        }
        "package_body" => {
            visit_container(
                builder,
                ContainerVisit {
                    node,
                    depth,
                    name: node.child_by_field_name("package"),
                    kind: SymbolKind::Module,
                    declaration_only: false,
                },
            )?;
        }
        "function_body"
        | "procedure_body"
        | "function_declaration"
        | "procedure_declaration"
        | "function_interface_declaration"
        | "procedure_interface_declaration" => visit_vhdl_callable(builder, node, depth)?,
        "full_type_declaration" | "incomplete_type_declaration" | "subtype_declaration" => {
            visit_type_declaration(builder, node, depth)?;
        }
        "constant_declaration" => {
            visit_vhdl_bindings(builder, node, SymbolKind::Constant)?;
        }
        "signal_declaration" => visit_vhdl_bindings(builder, node, SymbolKind::Field)?,
        "variable_declaration" | "shared_variable_declaration" => {
            visit_vhdl_bindings(builder, node, SymbolKind::Variable)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[derive(Clone, Copy)]
struct ContainerVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    name: Option<Node<'tree>>,
    kind: SymbolKind,
    declaration_only: bool,
}

fn visit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ContainerVisit<'_>,
) -> Result<Option<SymbolId>, ExtractError> {
    let Some(name_node) = input.name else {
        builder.visit_named_children(input.node, input.depth)?;
        return Ok(None);
    };
    let Some(name) = canonical_node_text(builder, name_node)? else {
        builder.visit_named_children(input.node, input.depth)?;
        return Ok(None);
    };
    let id = builder.emit_symbol(PendingSymbol {
        kind: input.kind,
        name: name.clone(),
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: (!input.declaration_only).then_some(input.node),
        declaration_only: input.declaration_only,
        signature: None,
        export: SymbolExportFlags::named(true),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    builder.owners.push(id.clone());
    builder.native_owner_kinds.push(input.kind);
    builder.qualifiers.push(name);
    let visit_result = builder.visit_named_children(input.node, input.depth);
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    visit_result?;
    Ok(Some(id))
}

fn visit_vhdl_architecture(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let entity = node.child_by_field_name("entity");
    let owner = visit_container(
        builder,
        ContainerVisit {
            node,
            depth,
            name: node.child_by_field_name("name"),
            kind: SymbolKind::Module,
            declaration_only: false,
        },
    )?;
    if let (Some(owner), Some(entity)) = (owner, entity) {
        emit_node_reference(
            builder,
            NodeReferenceEmission {
                owner: Some(owner),
                target: entity,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    Ok(())
}

fn visit_ada_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let specification = named_children(node).find(|child| {
        matches!(
            child.kind(),
            "function_specification" | "procedure_specification"
        )
    });
    let name = specification.and_then(|specification| specification.child_by_field_name("name"));
    let declaration_only = node.kind() == "subprogram_declaration";
    visit_callable(
        builder,
        CallableVisit {
            node,
            depth,
            name,
            declaration_only,
        },
    )
}

fn visit_vhdl_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let name = node.child_by_field_name("designator");
    let declaration_only = matches!(
        node.kind(),
        "function_declaration"
            | "procedure_declaration"
            | "function_interface_declaration"
            | "procedure_interface_declaration"
    );
    visit_callable(
        builder,
        CallableVisit {
            node,
            depth,
            name,
            declaration_only,
        },
    )
}

#[derive(Clone, Copy)]
struct CallableVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    name: Option<Node<'tree>>,
    declaration_only: bool,
}

fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: CallableVisit<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = input.name else {
        return builder.visit_named_children(input.node, input.depth);
    };
    let Some(name) = canonical_node_text(builder, name_node)? else {
        return builder.visit_named_children(input.node, input.depth);
    };
    let id = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Function,
        name: name.clone(),
        span_node: input.node,
        structural_node: input.node,
        doc_anchor: input.node,
        body_node: (!input.declaration_only).then_some(input.node),
        declaration_only: input.declaration_only,
        signature: None,
        export: SymbolExportFlags::named(true),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Function);
    builder.qualifiers.push(name);
    let result = builder.visit_named_children(input.node, input.depth);
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn visit_type_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let name = node
        .child_by_field_name("name")
        .or_else(|| first_direct_identifier(node));
    visit_container(
        builder,
        ContainerVisit {
            node,
            depth,
            name,
            kind: SymbolKind::TypeAlias,
            declaration_only: false,
        },
    )?;
    Ok(())
}

fn visit_ada_objects(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let kind = if builder
        .context
        .text(node)
        .split(':')
        .nth(1)
        .is_some_and(|tail| tail.to_ascii_lowercase().contains("constant"))
    {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    let mut cursor = node.walk();
    for name_node in node.children_by_field_name("name", &mut cursor) {
        emit_binding_symbol(
            builder,
            BindingSymbolEmission {
                declaration: node,
                name: name_node,
                kind,
            },
        )?;
    }
    Ok(())
}

fn visit_first_named_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: SymbolKind,
) -> Result<(), ExtractError> {
    if let Some(name) = first_direct_identifier(node) {
        emit_binding_symbol(
            builder,
            BindingSymbolEmission {
                declaration: node,
                name,
                kind,
            },
        )?;
    }
    Ok(())
}

fn visit_vhdl_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: SymbolKind,
) -> Result<(), ExtractError> {
    let Some(list) = named_children(node).find(|child| child.kind() == "identifier_list") else {
        return Ok(());
    };
    for name in named_children(list) {
        if matches!(name.kind(), "identifier" | "extended_identifier") {
            emit_binding_symbol(
                builder,
                BindingSymbolEmission {
                    declaration: node,
                    name,
                    kind,
                },
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct BindingSymbolEmission<'tree> {
    declaration: Node<'tree>,
    name: Node<'tree>,
    kind: SymbolKind,
}

fn emit_binding_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: BindingSymbolEmission<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = canonical_node_text(builder, input.name)? else {
        return Ok(());
    };
    builder.emit_symbol(PendingSymbol {
        kind: input.kind,
        name,
        span_node: input.name,
        structural_node: input.declaration,
        doc_anchor: input.declaration,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::named(true),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    Ok(())
}

fn capture_ada_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let target = match node.kind() {
        "function_call" | "procedure_call_statement" => node.child_by_field_name("name"),
        _ => None,
    };
    if let Some(target) = target {
        emit_node_reference(
            builder,
            NodeReferenceEmission {
                owner: builder.owners.last().cloned(),
                target,
                kind: ReferenceKind::Calls,
            },
        )?;
    }
    Ok(())
}

fn capture_vhdl_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let (target, kind) = match node.kind() {
        "function_call" => (node.child_by_field_name("function"), ReferenceKind::Calls),
        "ambiguous_name" if named_children(node).any(|child| child.kind() == "expression_list") => {
            (node.child_by_field_name("prefix"), ReferenceKind::Calls)
        }
        "procedure_call_statement" => (node.child_by_field_name("procedure"), ReferenceKind::Calls),
        "component_instantiation" => (
            node.child_by_field_name("component"),
            ReferenceKind::Instantiates,
        ),
        "entity_instantiation" => (
            node.child_by_field_name("entity"),
            ReferenceKind::Instantiates,
        ),
        "configuration_instantiation" => (
            node.child_by_field_name("configuration"),
            ReferenceKind::Instantiates,
        ),
        _ => (None, ReferenceKind::Calls),
    };
    if let Some(target) = target {
        emit_node_reference(
            builder,
            NodeReferenceEmission {
                owner: builder.owners.last().cloned(),
                target,
                kind,
            },
        )?;
    }
    Ok(())
}

fn visit_ada_context_clause(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    wildcard: bool,
) -> Result<(), ExtractError> {
    for target in named_children(node) {
        if !matches!(target.kind(), "identifier" | "selected_component") {
            continue;
        }
        let Some(module) = canonical_node_text(builder, target)? else {
            continue;
        };
        let local = if wildcard {
            "*".to_owned()
        } else {
            module.rsplit('.').next().unwrap_or(&module).to_owned()
        };
        ImportEmission {
            target,
            module,
            kind: ImportBindingKind::Namespace,
            imported: "*".to_owned(),
            local,
        }
        .emit(builder)?;
    }
    Ok(())
}

fn visit_vhdl_library_clause(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(list) = named_children(node).find(|child| child.kind() == "logical_name_list") else {
        return Ok(());
    };
    let mut cursor = list.walk();
    for target in list.children_by_field_name("library", &mut cursor) {
        let Some(name) = canonical_node_text(builder, target)? else {
            continue;
        };
        emit_import_reference(builder, target, format!("library:{name}"))?;
    }
    Ok(())
}

fn visit_vhdl_use_clause(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    for target in named_children(node).filter(|child| child.kind() == "selected_name") {
        let Some(raw) = canonical_node_text(builder, target)? else {
            continue;
        };
        let Some(parsed) = parse_vhdl_use_name(&raw) else {
            continue;
        };
        ImportEmission {
            target,
            module: parsed.module,
            kind: parsed.kind,
            imported: parsed.imported,
            local: parsed.local,
        }
        .emit(builder)?;
    }
    Ok(())
}

struct ParsedVhdlUse {
    module: String,
    kind: ImportBindingKind,
    imported: String,
    local: String,
}

fn parse_vhdl_use_name(raw: &str) -> Option<ParsedVhdlUse> {
    let mut components = raw.split('.').filter(|component| !component.is_empty());
    let first = components.next()?;
    let mut retained = Vec::new();
    if first != "work" {
        retained.push(first);
    }
    retained.extend(components);
    if retained.len() < 2 {
        return None;
    }
    let imported = retained.pop()?;
    let wildcard = imported == "all";
    let module = retained.join(".");
    if module.is_empty() {
        return None;
    }
    Some(ParsedVhdlUse {
        module,
        kind: if wildcard {
            ImportBindingKind::Namespace
        } else {
            ImportBindingKind::Named
        },
        imported: if wildcard {
            "*".to_owned()
        } else {
            imported.to_owned()
        },
        local: if wildcard {
            "*".to_owned()
        } else {
            imported.to_owned()
        },
    })
}

struct ImportEmission<'tree> {
    target: Node<'tree>,
    module: String,
    kind: ImportBindingKind,
    imported: String,
    local: String,
}

impl ImportEmission<'_> {
    fn emit(self, builder: &mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError> {
        let reference_name = self.module.clone();
        builder.emit_import_binding(ExtractedImportBinding {
            kind: self.kind,
            module_specifier: self.module,
            imported_name: self.imported,
            local_name: self.local,
            span: span_for(self.target)?,
        })?;
        emit_import_reference(builder, self.target, reference_name)
    }
}

/// Emit a root-scope import symbol and its file-level `Imports` reference.
pub(super) fn emit_import_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'_>,
    name: String,
) -> Result<(), ExtractError> {
    let symbol_name = name.clone();
    with_root_scope(builder, |builder| {
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Import,
            name: symbol_name,
            span_node: target,
            structural_node: target,
            doc_anchor: target,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: SymbolExportFlags::default(),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })
    })?;
    builder.emit_reference(ExtractedReference {
        owner: None,
        name,
        resolution_name: None,
        kind: ReferenceKind::Imports,
        span: span_for(target)?,
    })
}

struct NodeReferenceEmission<'tree> {
    owner: Option<SymbolId>,
    target: Node<'tree>,
    kind: ReferenceKind,
}

fn emit_node_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NodeReferenceEmission<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = canonical_node_text(builder, input.target)? else {
        return Ok(());
    };
    builder.emit_reference(ExtractedReference {
        owner: input.owner,
        name,
        resolution_name: None,
        kind: input.kind,
        span: span_for(input.target)?,
    })
}

fn canonical_node_text(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let raw = builder.context.text(node).trim();
    if raw.is_empty()
        || raw.len() > MAXIMUM_UNIT_NAME_BYTES
        || raw.contains(['\0', '\n', '\r', '\'', '"', '`'])
    {
        return Ok(None);
    }
    let canonical = if raw.contains('\\') {
        raw.to_owned()
    } else {
        raw.to_ascii_lowercase()
    };
    builder.context.copy_text(&canonical).map(Some)
}

fn first_direct_identifier(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node).find(|child| {
        matches!(
            child.kind(),
            "identifier" | "extended_identifier" | "extended_simple_name"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vhdl_work_library_is_removed_from_local_package_bindings() {
        let Some(parsed) = parse_vhdl_use_name("work.math_pkg.all") else {
            panic!("valid VHDL use name did not parse");
        };
        assert_eq!(parsed.module, "math_pkg");
        assert_eq!(parsed.kind, ImportBindingKind::Namespace);
        assert_eq!(parsed.local, "*");
    }

    #[test]
    fn vhdl_named_import_retains_the_selected_declaration() {
        let Some(parsed) = parse_vhdl_use_name("work.math_pkg.compute") else {
            panic!("valid VHDL use name did not parse");
        };
        assert_eq!(parsed.module, "math_pkg");
        assert_eq!(parsed.kind, ImportBindingKind::Named);
        assert_eq!(parsed.imported, "compute");
        assert_eq!(parsed.local, "compute");
    }
}
