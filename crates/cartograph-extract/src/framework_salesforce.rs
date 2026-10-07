//! Path-proven Aura actions and LWC component declarations and uses.

use cartograph_domain::{ReferenceKind, SymbolKind};
use std::collections::BTreeMap;
use tree_sitter::{Node, TreeCursor};

mod clients;
mod servers;

use crate::{
    ExtractError, SalesforceBundleKind,
    framework::{
        FrameworkBuilder, FrameworkReferenceInput, LandmarkInput, javascript_identifier_at,
    },
    salesforce_bundle,
};

const MAX_ROOT_WRAPPERS: usize = 4;
const ACTION_INDEX_ENTRY_BYTES: u64 = 128;

pub(crate) fn is_lwc_use(path: &str, name: &str) -> bool {
    name.starts_with("c-")
        && salesforce_bundle(path)
            .is_some_and(|bundle| bundle.kind == SalesforceBundleKind::LwcTemplate)
}

pub(crate) fn scan(builder: &mut FrameworkBuilder<'_, '_>) -> Result<(), ExtractError> {
    let Some(kind) = salesforce_bundle(builder.path()).map(|bundle| bundle.kind) else {
        return Ok(());
    };
    match kind {
        SalesforceBundleKind::AuraController
        | SalesforceBundleKind::AuraHelper
        | SalesforceBundleKind::AuraRenderer => aura_actions(builder)?,
        SalesforceBundleKind::LwcScript => lwc_component(builder)?,
        SalesforceBundleKind::LwcTemplate => {}
        SalesforceBundleKind::AuraMarkup => return Ok(()),
    }
    scan_uses(builder, kind)
}

fn aura_actions(builder: &mut FrameworkBuilder<'_, '_>) -> Result<(), ExtractError> {
    let Some(object) = builder.syntax_root().and_then(root_object) else {
        return Ok(());
    };
    let source = builder.source();
    let mut actions = BTreeMap::new();
    let mut cursor = object.walk();
    for pair in object.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if pair.kind() == "comment" {
            continue;
        }
        // A computed property or spread can replace any action. Count static
        // nonfunction values too, so an overwritten function never gets a target.
        let Some(name) = property_name(source, pair) else {
            return Ok(());
        };
        builder.bridge.reserve_working_bytes(
            ACTION_INDEX_ENTRY_BYTES
                + u64::try_from(name.len()).map_err(|_| ExtractError::OutputLimit)?,
        )?;
        actions
            .entry(name)
            .and_modify(|site| *site = None)
            .or_insert_with(|| action_pair(pair).map(|_| pair));
    }
    for (name, site) in actions {
        builder.bridge.charge_work(1)?;
        let Some(pair) = site else {
            continue;
        };
        publish_action(builder, (name, pair))?;
    }
    Ok(())
}

fn publish_action<'source>(
    builder: &mut FrameworkBuilder<'source, '_>,
    input: (&str, Node<'source>),
) -> Result<(), ExtractError> {
    let (name, pair) = input;
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Method,
        name: name.to_owned(),
        identity: name.to_owned(),
        start: pair.start_byte(),
        end: pair.end_byte(),
        body_search_text: format!("aura client action {name}"),
        target: None,
    })?;
    if let Some((_, function)) = action_pair(pair) {
        clients::scan(builder, function)?;
        servers::scan(builder, function)?;
    }
    Ok(())
}

fn property_name<'source>(source: &'source str, pair: Node<'_>) -> Option<&'source str> {
    if pair.kind() != "pair" {
        return None;
    }
    let key = pair.child_by_field_name("key")?;
    let raw = &source[key.byte_range()];
    match key.kind() {
        "property_identifier" => Some(raw),
        "string" if !raw.contains('\\') => raw.get(1..raw.len().checked_sub(1)?),
        _ => None,
    }
}

fn root_object(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_ROOT_WRAPPERS {
        if node.kind() == "object" {
            return Some(node);
        }
        if node.named_child_count() != 1 {
            return None;
        }
        node = node.named_child(0)?;
    }
    None
}

fn action_pair(pair: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    if pair.kind() != "pair" {
        return None;
    }
    let key = pair.child_by_field_name("key")?;
    let function = pair.child_by_field_name("value")?;
    (key.kind() == "property_identifier" && function.kind() == "function_expression")
        .then_some((key, function))
}

fn lwc_component(builder: &mut FrameworkBuilder<'_, '_>) -> Result<(), ExtractError> {
    let mut declaration = None;
    for index in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        let Some(symbol) = builder
            .original_symbol(index)
            .filter(|symbol| symbol.kind == SymbolKind::Class && symbol.export.default_export)
        else {
            continue;
        };
        if declaration.is_some() {
            return Ok(());
        }
        declaration = Some((symbol.span.start_byte(), symbol.span.end_byte()));
    }
    let Some((start, end)) = declaration else {
        return Ok(());
    };
    let Some(bundle) = salesforce_bundle(builder.path()) else {
        return Ok(());
    };
    let name = bundle.name.to_owned();
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Component,
        name: name.clone(),
        identity: name.clone(),
        start: usize::try_from(start).map_err(|_| ExtractError::InvalidSpan)?,
        end: usize::try_from(end).map_err(|_| ExtractError::InvalidSpan)?,
        body_search_text: format!("salesforce lwc component {name}"),
        target: None,
    })
}

fn scan_uses(
    builder: &mut FrameworkBuilder<'_, '_>,
    kind: SalesforceBundleKind,
) -> Result<(), ExtractError> {
    if kind != SalesforceBundleKind::LwcTemplate {
        return Ok(());
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(());
    };
    visit_nodes(builder, (root, false), lwc_tag)
}

fn visit_nodes<'source, 'cancel>(
    builder: &mut FrameworkBuilder<'source, 'cancel>,
    traversal: (Node<'source>, bool),
    mut visitor: impl FnMut(
        &mut FrameworkBuilder<'source, 'cancel>,
        Node<'source>,
    ) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    let (root, skip_nested) = traversal;
    let mut cursor = root.walk();
    loop {
        builder.bridge.charge_work(1)?;
        let node = cursor.node();
        let nested = skip_nested
            && node != root
            && matches!(
                node.kind(),
                "function_expression"
                    | "function_declaration"
                    | "arrow_function"
                    | "method_definition"
                    | "class"
            );
        if !nested {
            visitor(builder, node)?;
        }
        if !advance(&mut cursor, !nested) {
            return Ok(());
        }
    }
}

fn advance(cursor: &mut TreeCursor<'_>, descend: bool) -> bool {
    if descend && cursor.goto_first_child() {
        return true;
    }
    while !cursor.goto_next_sibling() {
        if !cursor.goto_parent() {
            return false;
        }
    }
    true
}

fn lwc_tag(builder: &mut FrameworkBuilder<'_, '_>, node: Node<'_>) -> Result<(), ExtractError> {
    if !matches!(node.kind(), "start_tag" | "self_closing_tag") {
        return Ok(());
    }
    let Some(tag) = node.named_child(0).filter(|tag| tag.kind() == "tag_name") else {
        return Ok(());
    };
    let raw = &builder.source()[tag.byte_range()];
    let Some(tail) = raw.strip_prefix("c-") else {
        return Ok(());
    };
    let Some(name) = camel_case(tail) else {
        return Ok(());
    };
    let resolution = format!("c/{name}");
    builder.add_reference(FrameworkReferenceInput {
        owner: None,
        name: &resolution,
        resolution_name: None,
        kind: ReferenceKind::References,
        start: tag.start_byte(),
        end: tag.end_byte(),
    })
}

fn camel_case(value: &str) -> Option<String> {
    let mut parts = value.split('-');
    let mut name = parts.next()?.to_owned();
    for part in parts {
        let mut characters = part.chars();
        name.extend(characters.next()?.to_uppercase());
        name.extend(characters);
    }
    javascript_identifier_at(&name, 0).filter(|(end, _)| *end == name.len())?;
    Some(name)
}
