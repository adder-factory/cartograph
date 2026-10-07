//! Only direct, unchanged action receivers establish a client convention.

use tree_sitter::Node;

use super::{FrameworkBuilder, visit_nodes};
use crate::{
    ExtractError, SALESFORCE_CLIENT_MODULE, SalesforceBundleKind, framework::literal_bindings,
    salesforce_bundle,
};

const HELPER_PARAMETER_INDEX: u32 = 2;

pub(super) fn scan<'source>(
    builder: &mut FrameworkBuilder<'source, '_>,
    function: Node<'source>,
) -> Result<(), ExtractError> {
    let parameter = function
        .child_by_field_name("parameters")
        .and_then(|parameters| parameters.named_child(HELPER_PARAMETER_INDEX))
        .filter(|parameter| {
            salesforce_bundle(builder.path())
                .is_some_and(|bundle| bundle.kind == SalesforceBundleKind::AuraController)
                && parameter.kind() == "identifier"
                && &builder.source()[parameter.byte_range()] == "helper"
        });
    let helper =
        parameter.is_some() && unchanged(builder, (function, "helper", parameter), call_receiver)?;
    let this = unchanged(builder, (function, "this", None), call_receiver)?;
    visit_nodes(builder, (function, true), |builder, node| {
        client_call(builder, (node, helper, this))
    })
}

pub(super) fn unchanged<'source>(
    builder: &mut FrameworkBuilder<'source, '_>,
    query: (Node<'source>, &str, Option<Node<'source>>),
    allowed: impl Fn(Node<'source>) -> bool,
) -> Result<bool, ExtractError> {
    let (function, receiver, parameter) = query;
    let mut unchanged = true;
    visit_nodes(builder, (function, false), |builder, node| {
        if dynamic_scope(builder.source(), node)
            || (matches!(
                node.kind(),
                "identifier" | "this" | "shorthand_property_identifier_pattern"
            ) && &builder.source()[node.byte_range()] == receiver
                && Some(node) != parameter
                && !allowed(node))
        {
            unchanged = false;
        }
        Ok(())
    })?;
    Ok(unchanged)
}

fn dynamic_scope(source: &str, node: Node<'_>) -> bool {
    // Parentheses do not make eval indirect. A syntactic eval identifier is
    // outside this exact subset, regardless of how its call is wrapped.
    node.kind() == "with_statement"
        || (node.kind() == "identifier" && &source[node.byte_range()] == "eval")
}

pub(super) fn call_receiver(node: Node<'_>) -> bool {
    let Some(member) = node
        .parent()
        .filter(|parent| parent.kind() == "member_expression")
    else {
        return false;
    };
    member.child_by_field_name("object") == Some(node)
        && member.parent().is_some_and(|call| {
            call.kind() == "call_expression" && call.child_by_field_name("function") == Some(member)
        })
}

fn client_call(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (Node<'_>, bool, bool),
) -> Result<(), ExtractError> {
    let (node, helper, this) = input;
    if node.kind() != "call_expression" {
        return Ok(());
    }
    let Some(member) = node
        .child_by_field_name("function")
        .filter(|member| member.kind() == "member_expression")
    else {
        return Ok(());
    };
    let Some(receiver) = member.child_by_field_name("object") else {
        return Ok(());
    };
    let enabled = match &builder.source()[receiver.byte_range()] {
        "helper" => helper,
        "this" => this,
        _ => false,
    };
    if !enabled
        || member
            .child_by_field_name("property")
            .is_none_or(|property| property.kind() != "property_identifier")
    {
        return Ok(());
    }
    literal_bindings::append(
        builder,
        (
            SALESFORCE_CLIENT_MODULE,
            &builder.source()[member.byte_range()],
            member.start_byte(),
            member.end_byte(),
        ),
    )
}
