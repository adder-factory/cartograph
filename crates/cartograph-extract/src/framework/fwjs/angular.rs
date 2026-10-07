//! Literal Angular route arrays, including route-owned lazy-module evidence.
use super::{first_argument, lexical, literal, text, walk};
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, FrameworkReferenceInput, LandmarkInput, safe_route_value},
};
use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use std::{collections::BTreeSet, mem::size_of};
use tree_sitter::Node;

const ROUTE_ARRAY_BYTES: usize = size_of::<usize>() * 4;

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let mut arrays = BTreeSet::new();
    walk(builder, root, |builder, node| {
        if node.kind() == "array"
            && (routes_array(builder, node) || child_array(builder, node, &arrays))
        {
            builder.bridge.reserve_working_bytes(
                u64::try_from(ROUTE_ARRAY_BYTES).map_err(|_| ExtractError::OutputLimit)?,
            )?;
            arrays.insert(node.id());
        } else if node.kind() == "object"
            && node
                .parent()
                .is_some_and(|parent| arrays.contains(&parent.id()))
        {
            scan_object(builder, node, lexical)?;
        }
        Ok(())
    })
}

fn routes_array(builder: &FrameworkBuilder<'_, '_>, array: Node<'_>) -> bool {
    let Some(parent) = array.parent() else {
        return false;
    };
    if parent.kind() == "variable_declarator" {
        return parent.child_by_field_name("type").is_some_and(|node| {
            text(builder, node).trim().trim_start_matches(':').trim() == "Routes"
        });
    }
    if parent.kind() != "arguments" {
        return false;
    }
    parent
        .parent()
        .and_then(|call| call.child_by_field_name("function"))
        .is_some_and(|function| {
            matches!(
                text(builder, function),
                "provideRouter" | "RouterModule.forRoot" | "RouterModule.forChild"
            )
        })
}

fn child_array(
    builder: &FrameworkBuilder<'_, '_>,
    array: Node<'_>,
    arrays: &BTreeSet<usize>,
) -> bool {
    let Some(pair) = array.parent().filter(|parent| parent.kind() == "pair") else {
        return false;
    };
    pair.child_by_field_name("key")
        .is_some_and(|key| text(builder, key) == "children")
        && pair
            .parent()
            .and_then(|object| object.parent())
            .is_some_and(|parent| arrays.contains(&parent.id()))
}

fn field<'tree>(
    object: Node<'tree>,
    name: &str,
    builder: &mut FrameworkBuilder<'_, '_>,
) -> Result<Option<Node<'tree>>, ExtractError> {
    let mut cursor = object.walk();
    let mut found = None;
    for pair in object.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if pair
            .child_by_field_name("key")
            .is_some_and(|key| text(builder, key) == name)
        {
            if found.is_some() {
                return Ok(None);
            }
            found = pair.child_by_field_name("value");
        }
    }
    Ok(found)
}

fn scan_object(
    builder: &mut FrameworkBuilder<'_, '_>,
    object: Node<'_>,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let Some(path_node) = field(object, "path", builder)? else {
        return Ok(());
    };
    let Some(raw) = literal(builder, path_node) else {
        return Ok(());
    };
    let path = if raw.starts_with('/') {
        raw.to_owned()
    } else {
        format!("/{raw}")
    };
    let Some(path) = safe_route_value(&path, false) else {
        return Ok(());
    };
    let Some(owner) = builder.add_signed_landmark(
        LandmarkInput {
            kind: SymbolKind::Route,
            name: path.clone(),
            identity: format!("angular::{path}"),
            start: path_node.start_byte(),
            end: path_node.end_byte(),
            body_search_text: format!("angular route {path}"),
            target: None,
        },
        format!("Angular route {path}"),
    )?
    else {
        return Ok(());
    };
    let site = RouteSite {
        owner,
        node: path_node,
    };
    if let Some(component) =
        field(object, "component", builder)?.filter(|node| node.kind() == "identifier")
    {
        add_component_reference(builder, &site, (component, lexical))?;
    }
    for key in ["loadChildren", "loadComponent"] {
        if let Some(value) = field(object, key, builder)? {
            scan_lazy(builder, &site, value)?;
        }
    }
    Ok(())
}

struct RouteSite<'tree> {
    owner: SymbolId,
    node: Node<'tree>,
}

fn add_component_reference(
    builder: &mut FrameworkBuilder<'_, '_>,
    site: &RouteSite<'_>,
    (component, lexical): (Node<'_>, &lexical::Index),
) -> Result<(), ExtractError> {
    let Some(resolution) = lexical.resolution(builder, component)? else {
        return Ok(());
    };
    builder.add_reference(FrameworkReferenceInput {
        owner: Some(site.owner.clone()),
        name: text(builder, component),
        kind: ReferenceKind::References,
        resolution_name: resolution.resolution_name.as_deref(),
        start: component.start_byte(),
        end: component.end_byte(),
    })
}

fn scan_lazy(
    builder: &mut FrameworkBuilder<'_, '_>,
    site: &RouteSite<'_>,
    value: Node<'_>,
) -> Result<(), ExtractError> {
    walk(builder, value, |builder, node| {
        if let Some(module) = import_module(builder, node) {
            builder.add_reference(FrameworkReferenceInput {
                owner: Some(site.owner.clone()),
                name: module,
                resolution_name: Some(&format!("framework-angular-lazy::{module}")),
                kind: ReferenceKind::Imports,
                start: site.node.start_byte(),
                end: site.node.end_byte(),
            })?;
        }
        if let Some((module, export)) = then_export(builder, node) {
            builder.add_reference(FrameworkReferenceInput {
                owner: Some(site.owner.clone()),
                name: export,
                resolution_name: Some(&format!("framework-angular-export::{module}#{export}")),
                kind: ReferenceKind::References,
                start: site.node.start_byte(),
                end: site.node.end_byte(),
            })?;
        }
        Ok(())
    })
}

fn then_export<'source>(
    builder: &FrameworkBuilder<'source, '_>,
    node: Node<'_>,
) -> Option<(&'source str, &'source str)> {
    if node.kind() != "arrow_function" {
        return None;
    }
    let arguments = node.parent()?;
    if arguments.kind() != "arguments" || arguments.named_child(0) != Some(node) {
        return None;
    }
    let function = arguments.parent()?.child_by_field_name("function")?;
    let property = function.child_by_field_name("property")?;
    if text(builder, property) != "then" {
        return None;
    }
    let module = import_module(builder, function.child_by_field_name("object")?)?;
    let body = node.child_by_field_name("body")?;
    let receiver = body.child_by_field_name("object")?;
    let parameter = node
        .child_by_field_name("parameter")
        .or_else(|| node.child_by_field_name("parameters")?.named_child(0))?;
    if receiver.kind() != "identifier" || text(builder, receiver) != text(builder, parameter) {
        return None;
    }
    Some((module, text(builder, body.child_by_field_name("property")?)))
}

fn import_module<'source>(
    builder: &FrameworkBuilder<'source, '_>,
    call: Node<'_>,
) -> Option<&'source str> {
    if call.kind() != "call_expression" || call.child_by_field_name("function")?.kind() != "import"
    {
        return None;
    }
    first_argument(call).and_then(|argument| literal(builder, argument))
}
