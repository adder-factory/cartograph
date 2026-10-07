//! Explicit pattern names narrow value shadow fences. Unknown syntax stays opaque.
use tree_sitter::Node;

use super::{rust_reads, rust_use_guards};
use crate::{
    ExtractError,
    walk::{ExtractionBuilder, syntax::descendants_including_root},
};

const MAX_PATTERN_NODES: usize = 4096;
const SITE_KINDS: [&str; 6] = [
    "let_declaration",
    "let_condition",
    "for_expression",
    "match_arm",
    "parameter",
    "closure_expression",
];
const LEAF_KINDS: [&str; 17] = [
    "type_identifier",
    "field_identifier",
    "scoped_identifier",
    "scoped_type_identifier",
    "wildcard_pattern",
    "remaining_field_pattern",
    "range_pattern",
    "integer_literal",
    "float_literal",
    "boolean_literal",
    "char_literal",
    "string_literal",
    "raw_string_literal",
    "negative_literal",
    "line_comment",
    "block_comment",
    "_",
];

pub(super) fn retain(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    if !SITE_KINDS.contains(&node.kind()) {
        return Ok(false);
    }
    let field = if node.kind() == "closure_expression" {
        "parameters"
    } else {
        "pattern"
    };
    let Some(pattern) = node.child_by_field_name(field) else {
        return Ok(false);
    };
    if !container(pattern) {
        return Ok(false);
    }
    for (position, candidate) in descendants_including_root(pattern).enumerate() {
        builder.context.ensure_active()?;
        let invalid = candidate.is_error() || candidate.is_missing();
        let unknown = candidate.is_named() && !supported_node(candidate);
        if position >= MAX_PATTERN_NODES || invalid || unknown {
            rust_use_guards::unrepresented_binding(builder, ("*".to_owned(), node))?;
            return Ok(true);
        }
        if rust_reads::PATTERN_BINDING_KINDS.contains(&candidate.kind()) {
            let name = builder.context.owned_text(candidate)?;
            rust_use_guards::unrepresented_binding(builder, (name, node))?;
        }
    }
    Ok(true)
}

fn supported_node(node: Node<'_>) -> bool {
    container(node)
        || rust_reads::PATTERN_BINDING_KINDS.contains(&node.kind())
        || LEAF_KINDS.contains(&node.kind())
}

fn container(node: Node<'_>) -> bool {
    rust_reads::NESTED_PATTERN_KINDS.contains(&node.kind())
        || matches!(node.kind(), "closure_parameters" | "match_pattern")
}
