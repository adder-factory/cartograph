//! Salesforce Apex extraction.
//!
//! The sfapex grammar is Java-shaped, so classes, interfaces, enums, methods,
//! constructors, fields, annotations, inheritance, and invocations go through the
//! managed (Java) family. This module adds the Apex-only constructs:
//!
//! - `trigger Name on SObject (before insert, ...) { ... }` becomes a function
//!   whose signature names the object and its events, owning the trigger body;
//! - inline SOQL/SOSL queries (`[SELECT Id FROM Account]`,
//!   `[FIND 'x' RETURNING Contact, Lead]`) reference the sObjects they read.

use cartograph_domain::{
    ReferenceKind, SymbolKind, Visibility, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, managed_family, references,
    syntax::{descendants, named_children},
};

const MAX_SIGNATURE_BYTES: usize = 512;
const MAX_OBJECT_NAME_BYTES: usize = 255;
/// Distinct sObjects recorded per query; a query naming more is truncated.
const MAX_QUERY_OBJECTS: usize = 64;

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if node.kind() == "trigger_declaration" {
        visit_trigger(builder, node, depth)?;
        return Ok(true);
    }
    let handled = managed_family::visit_declaration(builder, node, depth)?;
    if handled && node.kind() == "field_declaration" {
        visit_property_accessors(builder, node, depth)?;
    }
    Ok(handled)
}

/// Apex properties are fields followed by an `accessor_list`
/// (`Account row { get { return load(); } }`); the accessor bodies are walked
/// with the field as owner so their calls and queries are not lost.
fn visit_property_accessors(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(accessors) = named_children(node).find(|child| child.kind() == "accessor_list") else {
        return Ok(());
    };
    let Some(name_node) = node
        .child_by_field_name("declarator")
        .and_then(|declarator| declarator.child_by_field_name("name"))
    else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let qualified = builder.qualified_name(&name)?;
    let Some(field) = builder
        .facts
        .symbols
        .iter()
        .rev()
        .find(|symbol| symbol.kind == SymbolKind::Field && symbol.qualified_name == qualified)
        .map(|symbol| symbol.id.clone())
    else {
        return Ok(());
    };
    builder.owners.push(field);
    builder.native_owner_kinds.push(SymbolKind::Field);
    builder.qualifiers.push(name);
    let result = builder.visit(accessors, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() == "query_expression" {
        return capture_query_objects(builder, node);
    }
    managed_family::capture_usage(builder, node)
}

fn visit_trigger(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let body = node.child_by_field_name("body");
    let signature = trigger_signature(builder, node)?;
    let id = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Function,
        name: name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only: body.is_none(),
        signature,
        // A trigger is a platform entry point: it is invoked by DML on its
        // object, never by name, so it is always externally reachable.
        export: crate::SymbolExportFlags::new(true, false),
        async_symbol: false,
        static_member: false,
        visibility: Some(Visibility::Public),
    })?;
    let Some(body) = body else {
        return Ok(());
    };
    builder.owners.push(id);
    builder.native_owner_kinds.push(SymbolKind::Function);
    builder.qualifiers.push(name);
    let result = builder.visit(body, depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

/// `on Account (before insert, after update)` (v1 `apexTriggerSignature`).
fn trigger_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    builder.context.ensure_active()?;
    let object = node
        .child_by_field_name("object")
        .map(|object| builder.context.text(object).trim());
    let events = named_children(node)
        .filter(|child| child.kind() == "trigger_event")
        .map(|event| builder.context.text(event))
        .collect::<Vec<_>>();
    if object.is_none() && events.is_empty() {
        return Ok(None);
    }
    let mut signature = String::new();
    match object {
        Some(object) => {
            signature.push_str("on ");
            signature.push_str(object);
        }
        None => signature.push_str("trigger"),
    }
    for (index, event) in events.iter().enumerate() {
        signature.push_str(if index == 0 { " (" } else { ", " });
        push_event_words(&mut signature, event);
        if signature.len() > MAX_SIGNATURE_BYTES {
            return Ok(None);
        }
    }
    if !events.is_empty() {
        signature.push(')');
    }
    if signature.len() > MAX_SIGNATURE_BYTES || !callable_signature_is_literal_free(&signature) {
        return Ok(None);
    }
    builder.context.copy_text(&signature).map(Some)
}

/// `before insert` / `before_insert` both render as `before insert`.
fn push_event_words(signature: &mut String, event: &str) {
    let words = event
        .split(|character: char| character.is_whitespace() || character == '_')
        .filter(|word| !word.is_empty());
    for (index, word) in words.enumerate() {
        if index > 0 {
            signature.push(' ');
        }
        signature.push_str(word);
    }
}

/// Reference every distinct sObject a SOQL (`FROM`) or SOSL (`RETURNING`)
/// query reads, owned by the enclosing declaration.
fn capture_query_objects(
    builder: &mut ExtractionBuilder<'_, '_>,
    query: Node<'_>,
) -> Result<(), ExtractError> {
    let mut seen = Vec::new();
    for candidate in descendants(query) {
        builder.context.ensure_active()?;
        let Some(object) = query_object_node(candidate) else {
            continue;
        };
        let name = builder.context.text(object).trim();
        if !is_object_name(name) || seen.iter().any(|existing: &String| existing == name) {
            continue;
        }
        if seen.len() >= MAX_QUERY_OBJECTS {
            break;
        }
        let name = builder.context.copy_text(name)?;
        seen.push(name.clone());
        references::push_reference(
            builder,
            PendingReference {
                owner: builder.owners.last().cloned(),
                name,
                kind: ReferenceKind::References,
                node: object,
            },
        )?;
    }
    Ok(())
}

/// The object-name node of a SOQL `storage_identifier` or SOSL `sobject_return`.
fn query_object_node(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "storage_identifier" => Some(node),
        "sobject_return" => named_children(node).find(|child| child.kind() == "identifier"),
        _ => None,
    }
}

fn is_object_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_OBJECT_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
        && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
}
