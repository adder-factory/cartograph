//! Rust outer attributes as decorators of the item they precede.
//!
//! Each outer attribute written before an item that declares a symbol
//! (`#[test]`, `#[derive(Debug)]`, `#[tauri::command]`, `#[get("/x")]`) is a
//! `Decorates` reference from that symbol, named by the attribute path's final
//! segment (`command`) and looked up by the whole path (`tauri::command`).
//! Outer attributes are `attribute_item` siblings directly before the item, so
//! the scan walks back over them, skips comments between them, and stops at the
//! first other node: an earlier item's attributes never leak onto a later one.
//! Inner attributes (`#![..]`) configure the enclosing module rather than an
//! item and record nothing. An attribute whose path is not a plain name path,
//! such as a macro metavariable (`#[$meta]`), records nothing, and attribute
//! arguments never produce references.

use cartograph_domain::{ReferenceKind, SymbolId};
use tree_sitter::Node;

use super::type_targets::{NamedTarget, emit_leaf_reference};
use crate::{
    ExtractError,
    walk::{ExtractionBuilder, syntax::named_children},
};

/// Most outer attributes recorded for one item; real items carry a handful,
/// and the bound keeps a pathological attribute run from scanning unbounded.
const MAX_ITEM_ATTRIBUTES: usize = 64;

/// Item kinds whose declared symbol an outer attribute decorates.
const DECORATED_ITEM_KINDS: [&str; 10] = [
    "function_item",
    "function_signature_item",
    "struct_item",
    "enum_item",
    "enum_variant",
    "trait_item",
    "mod_item",
    "type_item",
    "const_item",
    "static_item",
];

/// Whether an outer attribute before a node of this kind decorates its symbol.
pub(super) fn decorates_item(kind: &str) -> bool {
    DECORATED_ITEM_KINDS.contains(&kind)
}

/// Record each outer attribute before `item` as decorating `owner`, in source order.
pub(super) fn capture_item_attributes(
    builder: &mut ExtractionBuilder<'_, '_>,
    item: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(());
    }
    let mut attributes = Vec::new();
    let mut sibling = item.prev_named_sibling();
    while let Some(candidate) = sibling {
        builder.context.ensure_active()?;
        if candidate.is_extra() {
            sibling = candidate.prev_named_sibling();
            continue;
        }
        if candidate.kind() != "attribute_item" || attributes.len() >= MAX_ITEM_ATTRIBUTES {
            break;
        }
        attributes
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        attributes.push(candidate);
        sibling = candidate.prev_named_sibling();
    }
    for attribute in attributes.into_iter().rev() {
        let Some(target) = attribute_target(attribute) else {
            continue;
        };
        emit_leaf_reference(
            builder,
            Some(owner.clone()),
            target.reference(ReferenceKind::Decorates),
        )?;
    }
    Ok(())
}

/// The path an `attribute_item` names: `test` or the leaf `main` of `actix_web::main`.
fn attribute_target(attribute_item: Node<'_>) -> Option<NamedTarget<'_>> {
    let attribute = named_children(attribute_item).find(|child| child.kind() == "attribute")?;
    let path = named_children(attribute).find(|child| !child.is_extra())?;
    match path.kind() {
        "identifier" => Some(NamedTarget::unqualified(path)),
        "scoped_identifier" => Some(NamedTarget {
            path,
            leaf: path.child_by_field_name("name")?,
        }),
        _ => None,
    }
}
