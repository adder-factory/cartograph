//! Syntax-proven shadows omitted by the v1-compatible symbol projection.

use cartograph_domain::{ReferenceKind, SourceLanguage};
use tree_sitter::Node;

use crate::ExtractError;

use super::{ExtractionBuilder, PendingReference, syntax::named_children};

const MAX_BINDINGS: usize = 64;

pub(super) enum BoundedNode<'tree> {
    Absent,
    Found(Node<'tree>),
    Truncated,
}

pub(super) fn bounded_named_child<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: (Node<'tree>, &[&str]),
) -> Result<BoundedNode<'tree>, ExtractError> {
    let (node, kinds) = input;
    for (position, child) in named_children(node).enumerate() {
        builder.context.ensure_active()?;
        if position >= MAX_BINDINGS {
            return Ok(BoundedNode::Truncated);
        }
        if kinds.contains(&child.kind()) {
            return Ok(BoundedNode::Found(child));
        }
    }
    Ok(BoundedNode::Absent)
}

pub(super) fn shadows_receiver(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<bool, ExtractError> {
    let language = builder.context.snapshot.language();
    let kotlin_construction =
        language == SourceLanguage::Kotlin && pending.kind == ReferenceKind::Instantiates;
    if !(pending.kind == ReferenceKind::Calls || kotlin_construction)
        || !matches!(
            language,
            SourceLanguage::Java | SourceLanguage::Kotlin | SourceLanguage::CSharp
        )
    {
        return Ok(false);
    }
    let name = match pending.name.split_once('.') {
        Some((head, _)) => head,
        None if kotlin_construction => &pending.name,
        None => return Ok(false),
    };
    let mut scope = pending.node.parent();
    for _ in 0..MAX_BINDINGS {
        builder.context.ensure_active()?;
        let Some(node) = scope else {
            return Ok(false);
        };
        match scope_parameters(builder, (node, pending.node))? {
            BoundedNode::Truncated => return Ok(true),
            BoundedNode::Found(parameters) if binds_name(builder, (parameters, name), 0)? => {
                return Ok(true);
            }
            BoundedNode::Absent | BoundedNode::Found(_) => {}
        }
        scope = node.parent();
    }
    Ok(true)
}

fn scope_parameters<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: (Node<'tree>, Node<'_>),
) -> Result<BoundedNode<'tree>, ExtractError> {
    let (node, reference) = input;
    if node.kind() == "enhanced_for_statement" {
        return Ok(enhanced_for_binding(node, reference));
    }
    if node.kind() == "catch_block" {
        return Ok(BoundedNode::Found(node));
    }
    if !matches!(
        node.kind(),
        "method_declaration"
            | "constructor_declaration"
            | "function_declaration"
            | "secondary_constructor"
            | "class_declaration"
            | "record_declaration"
            | "local_function_statement"
            | "anonymous_method_expression"
            | "anonymous_function"
            | "lambda_expression"
            | "lambda_literal"
            | "catch_clause"
    ) {
        return Ok(BoundedNode::Absent);
    }
    if let Some(parameters) = node.child_by_field_name("parameters") {
        return Ok(BoundedNode::Found(parameters));
    }
    bounded_named_child(
        builder,
        (
            node,
            &[
                "function_value_parameters",
                "primary_constructor",
                "lambda_parameters",
                "catch_formal_parameter",
                "catch_declaration",
            ],
        ),
    )
}

fn enhanced_for_binding<'tree>(node: Node<'tree>, reference: Node<'_>) -> BoundedNode<'tree> {
    let Some(body) = node.child_by_field_name("body") else {
        return BoundedNode::Truncated;
    };
    if reference.start_byte() < body.start_byte() || body.end_byte() < reference.end_byte() {
        return BoundedNode::Absent;
    }
    node.child_by_field_name("name")
        .map_or(BoundedNode::Truncated, BoundedNode::Found)
}

fn binds_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: (Node<'_>, &str),
    depth: usize,
) -> Result<bool, ExtractError> {
    let (node, name) = input;
    if matches!(
        node.kind(),
        "identifier" | "simple_identifier" | "implicit_parameter"
    ) {
        return Ok(builder.context.text(node) == name);
    }
    if let Some(declared) = node.child_by_field_name("name") {
        return Ok(builder.context.text(declared) == name);
    }
    if depth >= 4 {
        return Ok(true);
    }
    for (position, child) in named_children(node).enumerate() {
        builder.context.ensure_active()?;
        if position >= MAX_BINDINGS {
            return Ok(true);
        }
        if binding_part(child.kind()) && binds_name(builder, (child, name), depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn binding_part(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "simple_identifier"
            | "implicit_parameter"
            | "parameter"
            | "formal_parameter"
            | "spread_parameter"
            | "class_parameter"
            | "variable_declarator"
            | "variable_declaration"
            | "multi_variable_declaration"
    )
}
