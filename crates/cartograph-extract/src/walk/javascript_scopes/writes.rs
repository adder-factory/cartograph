//! File-wide writes withhold added local-constructor proof, preserving bindings.

use super::super::ExtractionBuilder;
use crate::ExtractError;
use std::collections::HashSet;
use tree_sitter::Node;

const WRITE_NAME_ALLOWANCE: u64 = 128;
const MAX_TARGET_HOPS: usize = crate::MAXIMUM_AST_DEPTH;

#[derive(Default)]
pub(super) struct Writes<'source> {
    names: HashSet<&'source str>,
    fenced: bool,
    complete: bool,
}

impl<'source> Writes<'source> {
    pub(super) fn observe(
        &mut self,
        builder: &mut ExtractionBuilder<'source, '_>,
        node: Node<'_>,
    ) -> Result<(), ExtractError> {
        builder.context.ensure_active()?;
        if node.parent().is_none() {
            self.complete = whole_source(builder, node);
        }
        self.fenced |= node.is_error() || node.is_missing() || dynamic_scope(builder, node);
        if !matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) || !written_target(builder, node)?
        {
            return Ok(());
        }
        let Some(name) = builder.context.snapshot.source().get(node.byte_range()) else {
            self.fenced = true;
            return Ok(());
        };
        if !self.names.contains(name) {
            builder
                .context
                .budget
                .reserve_working_bytes(WRITE_NAME_ALLOWANCE)?;
            self.names
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            self.names.insert(name);
        }
        Ok(())
    }

    pub(super) fn proven(&self, name: &str) -> bool {
        self.complete && !self.fenced && !self.names.contains(name)
    }
}

fn whole_source(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    let root = super::tree_root(node);
    let source = builder.context.snapshot.source();
    source
        .get(..root.start_byte())
        .is_some_and(|prefix| prefix.trim().is_empty())
        && source
            .get(root.end_byte()..)
            .is_some_and(|suffix| suffix.trim().is_empty())
}

fn dynamic_scope(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    node.kind() == "with_statement"
        || (node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .is_some_and(|callee| builder.context.text(callee) == "eval"))
}

fn written_target(
    builder: &mut ExtractionBuilder<'_, '_>,
    mut node: Node<'_>,
) -> Result<bool, ExtractError> {
    for _ in 0..MAX_TARGET_HOPS {
        builder.context.ensure_active()?;
        let Some(parent) = node.parent() else {
            return Ok(false);
        };
        match parent.kind() {
            "assignment_expression"
            | "augmented_assignment_expression"
            | "assignment_pattern"
            | "object_assignment_pattern" => {
                if parent.child_by_field_name("left") != Some(node) {
                    return Ok(false);
                }
                if parent.kind().ends_with("expression") {
                    return Ok(true);
                }
            }
            "pair_pattern" if parent.child_by_field_name("value") == Some(node) => {}
            "array_pattern"
            | "object_pattern"
            | "rest_pattern"
            | "parenthesized_expression"
            | "non_null_expression"
            | "as_expression"
            | "satisfies_expression"
            | "type_assertion" => {}
            "update_expression" => return Ok(parent.child_by_field_name("argument") == Some(node)),
            "for_in_statement" => return Ok(parent.child_by_field_name("left") == Some(node)),
            _ => return Ok(false),
        }
        node = parent;
    }
    Ok(true)
}
