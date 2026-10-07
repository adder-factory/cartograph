//! `NeuG` resources require a `NeuG` constructor or an explicit import.
mod bindings;
use super::{first_argument, literal, text, walk};
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, LandmarkInput},
};
use cartograph_domain::SymbolKind;
use std::{collections::BTreeSet, mem::size_of};
use tree_sitter::Node;

const CONSTRUCTORS: &[&str] = &[
    "Graph",
    "Database",
    "Vertex",
    "Node",
    "Edge",
    "Relationship",
];
const SET_ENTRY_BYTES: usize = size_of::<String>() + size_of::<usize>() * 4;

#[derive(Default)]
struct Imports<'source> {
    modules: BTreeSet<&'source str>,
    constructors: BTreeSet<&'source str>,
    blocked: BTreeSet<String>,
}

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    let mut imports = Imports::default();
    walk(builder, root, |builder, node| {
        if matches!(node.kind(), "import_statement" | "import_from_statement")
            && node.parent() == Some(root)
        {
            collect_import(builder, node, &mut imports)?;
        }
        Ok(())
    })?;
    bindings::block_shadowed_names(builder, &mut imports)?;
    let mut resources = BTreeSet::new();
    walk(builder, root, |builder, node| {
        if node.kind() == "call" {
            scan_call(builder, node, (&imports, &mut resources))?;
        }
        Ok(())
    })
}

fn collect_import<'source>(
    builder: &mut FrameworkBuilder<'source, '_>,
    node: Node<'_>,
    imports: &mut Imports<'source>,
) -> Result<(), ExtractError> {
    if node.kind() == "import_from_statement" {
        return collect_constructors(builder, node, imports);
    }
    let mut cursor = node.walk();
    for name in node.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        let (module, alias) = if name.kind() == "aliased_import" {
            (
                name.child_by_field_name("name"),
                name.child_by_field_name("alias"),
            )
        } else {
            (Some(name), Some(name))
        };
        if module.is_some_and(|module| text(builder, module) == "neug")
            && let Some(alias) = alias
        {
            reserve_entry(builder, text(builder, alias))?;
            imports.modules.insert(text(builder, alias));
        }
    }
    Ok(())
}

fn collect_constructors<'source>(
    builder: &mut FrameworkBuilder<'source, '_>,
    node: Node<'_>,
    imports: &mut Imports<'source>,
) -> Result<(), ExtractError> {
    if node
        .child_by_field_name("module_name")
        .is_none_or(|module| text(builder, module) != "neug")
    {
        return Ok(());
    }
    let mut cursor = node.walk();
    for name in node.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if CONSTRUCTORS.contains(&text(builder, name)) {
            reserve_entry(builder, text(builder, name))?;
            imports.constructors.insert(text(builder, name));
        }
    }
    Ok(())
}

fn reserve_entry(builder: &mut FrameworkBuilder<'_, '_>, name: &str) -> Result<(), ExtractError> {
    let bytes = SET_ENTRY_BYTES.saturating_add(name.len());
    builder
        .bridge
        .reserve_working_bytes(u64::try_from(bytes).map_err(|_| ExtractError::OutputLimit)?)
}

fn scan_call<'source>(
    builder: &mut FrameworkBuilder<'source, '_>,
    call: Node<'_>,
    (imports, resources): (&Imports<'source>, &mut BTreeSet<String>),
) -> Result<(), ExtractError> {
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(());
    };
    let name = text(builder, function);
    if imports
        .blocked
        .contains(name.split('.').next().unwrap_or(name))
        && !name.starts_with("neug.")
    {
        return Ok(());
    }
    let constructor = if let Some((module, constructor)) = name.split_once('.') {
        if module != "neug" && !imports.modules.contains(module) {
            return Ok(());
        }
        constructor
    } else {
        if !imports.constructors.contains(name) {
            return Ok(());
        }
        name
    };
    if !CONSTRUCTORS.contains(&constructor) {
        return Ok(());
    }
    let Some(argument) = first_argument(call) else {
        return Ok(());
    };
    let Some(value) = literal(builder, argument) else {
        return Ok(());
    };
    let name = format!("neug:{}:{value}", constructor.to_ascii_lowercase());
    if resources.contains(&name) {
        return Ok(());
    }
    reserve_entry(builder, &name)?;
    resources.insert(name.clone());
    builder.add_signed_landmark(
        LandmarkInput {
            kind: SymbolKind::Resource,
            name: name.clone(),
            identity: format!("resource::{name}"),
            start: argument.start_byte() + 1,
            end: argument.end_byte() - 1,
            body_search_text: format!("neug {constructor} {value}"),
            target: None,
        },
        format!("NeuG {constructor}"),
    )?;
    Ok(())
}
