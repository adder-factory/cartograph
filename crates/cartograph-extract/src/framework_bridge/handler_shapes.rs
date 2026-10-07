//! Handler qualification requires a simple, unique receiver; instance members abstain.

use tree_sitter::Node;

use super::{BTreeMap, ExtractError, FrameworkBuilder};

const MAX_SYNTAX_ANCESTORS: usize = 16;

pub(super) fn lookup(
    builder: &mut FrameworkBuilder<'_, '_>,
    (handler, unique): (Node<'_>, &BTreeMap<String, usize>),
) -> Result<Option<String>, ExtractError> {
    let Some(object) = handler.child_by_field_name("object") else {
        return Ok(None);
    };
    if object.kind() == "this" {
        // Instance/prototype writes can replace a method without rebinding the class.
        return Ok(None);
    }
    let Some(root) = member_root(builder, handler)? else {
        return Ok(None);
    };
    let source = builder.source();
    if unique.get(source.get(root.byte_range()).unwrap_or_default()) != Some(&1) {
        return Ok(None);
    }
    let Some(value) = source.get(handler.byte_range()) else {
        return Ok(None);
    };
    signal(builder, value)
}

fn signal(
    builder: &mut FrameworkBuilder<'_, '_>,
    value: &str,
) -> Result<Option<String>, ExtractError> {
    builder.bridge.charge_work(value.len())?;
    builder.bridge.reserve_working_bytes(
        u64::try_from(value.len()).map_err(|_| ExtractError::OutputLimit)?,
    )?;
    Ok(crate::framework::safe_signal(value))
}

fn member_root<'tree>(
    builder: &mut FrameworkBuilder<'_, '_>,
    mut node: Node<'tree>,
) -> Result<Option<Node<'tree>>, ExtractError> {
    for _ in 0..MAX_SYNTAX_ANCESTORS {
        builder.bridge.charge_work(1)?;
        if node.kind() == "identifier" {
            return Ok(Some(node));
        }
        if node.kind() != "member_expression"
            || node
                .child_by_field_name("property")
                .is_none_or(|property| property.kind() != "property_identifier")
        {
            return Ok(None);
        }
        let Some(object) = node.child_by_field_name("object") else {
            return Ok(None);
        };
        node = object;
    }
    Ok(None)
}
