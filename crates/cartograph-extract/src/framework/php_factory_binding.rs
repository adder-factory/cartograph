//! A member call immediately following an assignment from a static factory.
//! No flow inference: both statements must be adjacent in the same block.

use std::collections::BTreeMap;

use cartograph_domain::{ReferenceKind, SourceLanguage};
use tree_sitter::Node;

use super::{FrameworkBuilder, FrameworkNearReferenceInput, syntax_nodes::SyntaxNodes};
use crate::{ExtractError, PHP_EXACT_RESOLUTION_PREFIX};

const REFERENCE_INDEX_BYTES: u64 = 128;
const MAX_LOOKUP_BYTES: usize = 1_024;

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Php {
        return Ok(());
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(());
    };
    let references = factory_references(builder)?;
    for call in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        if call.kind() != "member_call_expression" {
            continue;
        }
        let Some((factory, member)) = adjacent_factory(source, call) else {
            continue;
        };
        let Some(key) = factory_key(builder, &references, factory)? else {
            continue;
        };
        let Some(name) = source.get(member.byte_range()) else {
            continue;
        };
        let resolution = format!("{PHP_EXACT_RESOLUTION_PREFIX}returned-member::{key}::{name}");
        builder.add_reference_near(FrameworkNearReferenceInput {
            name,
            resolution_name: Some(&resolution),
            kind: ReferenceKind::Calls,
            start: member.start_byte(),
            end: member.end_byte(),
        })?;
    }
    Ok(())
}

fn factory_key<'a>(
    builder: &'a FrameworkBuilder<'_, '_>,
    references: &BTreeMap<u64, usize>,
    factory: Node<'_>,
) -> Result<Option<&'a str>, ExtractError> {
    let Some(factory_name) = factory.child_by_field_name("name") else {
        return Ok(None);
    };
    let start = u64::try_from(factory_name.start_byte()).map_err(|_| ExtractError::OutputLimit)?;
    let Some(&position) = references.get(&start) else {
        return Ok(None);
    };
    Ok(builder.references()[position]
        .resolution_name
        .as_deref()
        .and_then(|name| name.strip_prefix(PHP_EXACT_RESOLUTION_PREFIX))
        .and_then(|lookup| lookup.strip_prefix("member::"))
        .filter(|key| key.len() <= MAX_LOOKUP_BYTES))
}

fn factory_references(
    builder: &mut FrameworkBuilder<'_, '_>,
) -> Result<BTreeMap<u64, usize>, ExtractError> {
    let count = builder.references().len();
    builder.bridge.reserve_working_bytes(
        REFERENCE_INDEX_BYTES
            .saturating_mul(u64::try_from(count).map_err(|_| ExtractError::OutputLimit)?),
    )?;
    let mut index = BTreeMap::new();
    for position in 0..count {
        builder.bridge.charge_work(1)?;
        let reference = &builder.references()[position];
        if reference.kind == ReferenceKind::Calls
            && reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.starts_with(PHP_EXACT_RESOLUTION_PREFIX))
        {
            index.insert(reference.span.start_byte(), position);
        }
    }
    Ok(index)
}

fn adjacent_factory<'tree>(source: &str, call: Node<'tree>) -> Option<(Node<'tree>, Node<'tree>)> {
    let object = call
        .child_by_field_name("object")
        .filter(|object| object.kind() == "variable_name")?;
    let member = call
        .child_by_field_name("name")
        .filter(|name| name.kind() == "name")?;
    let statement = call
        .parent()
        .filter(|statement| statement.kind() == "expression_statement")?;
    let previous = statement
        .prev_named_sibling()
        .filter(|previous| previous.kind() == "expression_statement")?;
    let assignment = previous
        .named_child(0)
        .filter(|assignment| assignment.kind() == "assignment_expression")?;
    let variable = assignment
        .child_by_field_name("left")
        .filter(|variable| variable.kind() == "variable_name")?;
    if source.get(variable.byte_range()) != source.get(object.byte_range()) {
        return None;
    }
    let factory = assignment
        .child_by_field_name("right")
        .filter(|factory| factory.kind() == "scoped_call_expression")?;
    Some((factory, member))
}
