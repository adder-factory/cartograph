//! Literal server actions require a stable component parameter in an Aura action.

use tree_sitter::Node;

use super::{
    FrameworkBuilder, FrameworkReferenceInput, ReferenceKind, clients, javascript_identifier_at,
    visit_nodes,
};
use crate::{ExtractError, SALESFORCE_CONTROLLER_MODULE, SalesforceBundleKind, salesforce_bundle};

const COMPONENT_PARAMETER_INDEX: u32 = 0;

pub(super) fn scan<'source>(
    builder: &mut FrameworkBuilder<'source, '_>,
    function: Node<'source>,
) -> Result<(), ExtractError> {
    if salesforce_bundle(builder.path())
        .is_none_or(|bundle| bundle.kind != SalesforceBundleKind::AuraController)
    {
        return Ok(());
    }
    let Some(parameter) = function
        .child_by_field_name("parameters")
        .and_then(|parameters| parameters.named_child(COMPONENT_PARAMETER_INDEX))
        .filter(|parameter| parameter.kind() == "identifier")
    else {
        return Ok(());
    };
    let receiver = &builder.source()[parameter.byte_range()];
    if !matches!(receiver, "component" | "cmp")
        || !clients::unchanged(
            builder,
            (function, receiver, Some(parameter)),
            component_read,
        )?
    {
        return Ok(());
    }
    visit_nodes(builder, (function, true), |builder, node| {
        server_action(builder, (node, receiver))
    })
}

// Passing a component as an argument cannot replace this local binding. Other
// reads are outside this exact subset; declarations and writes always abstain.
fn component_read(node: Node<'_>) -> bool {
    clients::call_receiver(node)
        || node
            .parent()
            .is_some_and(|parent| parent.kind() == "arguments")
}

fn server_action(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (Node<'_>, &str),
) -> Result<(), ExtractError> {
    let Some(literal) = server_action_literal(builder.source(), input) else {
        return Ok(());
    };
    let value = &builder.source()[literal.byte_range()];
    let Some(name) = value
        .get(1..value.len().saturating_sub(1))
        .filter(|value| value.starts_with("c."))
    else {
        return Ok(());
    };
    let action = &name["c.".len()..];
    if javascript_identifier_at(action, 0).is_none_or(|(end, _)| end != action.len()) {
        return Ok(());
    }
    let resolution = format!("{SALESFORCE_CONTROLLER_MODULE}::{action}");
    builder.add_reference(FrameworkReferenceInput {
        owner: None,
        name,
        resolution_name: Some(&resolution),
        kind: ReferenceKind::Calls,
        start: literal.start_byte() + 1,
        end: literal.end_byte() - 1,
    })
}

fn server_action_literal<'tree>(source: &str, input: (Node<'tree>, &str)) -> Option<Node<'tree>> {
    let (node, receiver) = input;
    if node.kind() != "call_expression" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    if function.kind() != "member_expression" {
        return None;
    }
    let object = function.child_by_field_name("object")?;
    let property = function.child_by_field_name("property")?;
    if object.kind() != "identifier"
        || &source[object.byte_range()] != receiver
        || &source[property.byte_range()] != "get"
    {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    let literal = arguments.named_child(0)?;
    (arguments.named_child_count() == 1 && literal.kind() == "string").then_some(literal)
}
