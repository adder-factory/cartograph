//! Explicit instance-property evidence vetoes resource conventions.

use std::collections::BTreeSet;

use tree_sitter::Node;

use crate::{
    ExtractError,
    framework::{FrameworkBuilder, syntax_nodes::SyntaxNodes},
};

const PROPERTY_ENTRY_BYTES: u64 = 192;
const MAX_PROPERTY_BYTES: usize = 1_024;
const MAX_WRITE_WRAPPERS: usize = 4;
const MAX_RECEIVER_WRAPPERS: usize = 4;

enum Effect<'s> {
    None,
    Property(&'s str),
    Opaque,
}

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    root: Node<'_>,
) -> Result<Option<BTreeSet<String>>, ExtractError> {
    let mut properties = BTreeSet::new();
    for node in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        match effect(source, node) {
            Effect::None => {}
            Effect::Opaque => return Ok(None),
            Effect::Property(name) => {
                if name.len() > MAX_PROPERTY_BYTES {
                    return Ok(None);
                }
                builder.bridge.charge_work(name.len())?;
                builder
                    .bridge
                    .reserve_working_bytes(PROPERTY_ENTRY_BYTES.saturating_add(
                        u64::try_from(name.len()).map_err(|_| ExtractError::OutputLimit)?,
                    ))?;
                properties.insert(name.to_owned());
            }
        }
    }
    Ok(Some(properties))
}

fn effect<'s>(source: &'s str, node: Node<'_>) -> Effect<'s> {
    if matches!(
        node.kind(),
        "property_element" | "property_promotion_parameter"
    ) {
        return node
            .child_by_field_name("name")
            .and_then(|name| source.get(name.byte_range()))
            .map_or(Effect::Opaque, |name| {
                Effect::Property(name.trim_start_matches(['&', '$']))
            });
    }
    if node.kind() != "member_access_expression" || !written(node) {
        return Effect::None;
    }
    match instance_receiver(source, node) {
        Some(true) => {}
        Some(false) => return Effect::None,
        None => return Effect::Opaque,
    }
    node.child_by_field_name("name")
        .filter(|name| name.kind() == "name")
        .and_then(|name| source.get(name.byte_range()))
        .map_or(Effect::Opaque, Effect::Property)
}

fn instance_receiver(source: &str, node: Node<'_>) -> Option<bool> {
    let mut receiver = node.child_by_field_name("object")?;
    for _ in 0..MAX_RECEIVER_WRAPPERS {
        if receiver.kind() != "parenthesized_expression" {
            return Some(source.get(receiver.byte_range()) == Some("$this"));
        }
        receiver = receiver.named_child(0).filter(|child| !child.is_extra())?;
    }
    None
}

fn written(mut node: Node<'_>) -> bool {
    for _ in 0..MAX_WRITE_WRAPPERS {
        let Some(parent) = node.parent() else {
            return false;
        };
        match parent.kind() {
            "assignment_expression"
            | "augmented_assignment_expression"
            | "reference_assignment_expression" => {
                return parent.child_by_field_name("left") == Some(node);
            }
            "update_expression" => return parent.child_by_field_name("argument") == Some(node),
            "unset_statement" => return true,
            "foreach_statement" => return parent.named_child(0) != Some(node),
            "subscript_expression"
            | "parenthesized_expression"
            | "list_literal"
            | "pair"
            | "by_ref" => node = parent,
            _ => return false,
        }
    }
    true
}
