//! Receiver scope evidence, including anonymous catch/destructuring bindings.

use super::{
    ExtractionBuilder,
    javascript_scopes::{self, NearestBinding},
};
use crate::{
    ExtractError, JavascriptMemberCallContext, JavascriptMemberReceiver,
    budget::javascript_call_context_budget_bytes,
};
use tree_sitter::Node;

const MAX_RECEIVER_DEPTH: usize = 64;

pub(super) fn capture(
    builder: &mut ExtractionBuilder<'_, '_>,
    (target, end_byte): (Node<'_>, u64),
) -> Result<(), ExtractError> {
    if builder
        .facts
        .javascript_member_calls
        .last()
        .is_some_and(|call| call.end_byte == end_byte)
    {
        return Ok(());
    }
    let Some(receiver) = receiver_context(builder, target)? else {
        return Ok(());
    };
    let context = JavascriptMemberCallContext { end_byte, receiver };
    builder.context.budget.reserve_fact(
        javascript_call_context_budget_bytes(&context),
        std::iter::empty(),
    )?;
    builder
        .facts
        .javascript_member_calls
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.facts.javascript_member_calls.push(context);
    Ok(())
}

fn receiver_context(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'_>,
) -> Result<Option<JavascriptMemberReceiver>, ExtractError> {
    if !matches!(target.kind(), "member_expression" | "subscript_expression") {
        return Ok(None);
    }
    let Some(root) = receiver_root(target) else {
        return Ok(None);
    };
    let constructor = root.kind() == "new_expression";
    // A member of a returned child is not a member of the constructed class.
    if constructor && target.child_by_field_name("object") != Some(root) {
        return Ok(None);
    }
    let receiver = if constructor {
        root.child_by_field_name("constructor")
    } else {
        Some(root)
    };
    let Some(receiver) = receiver.filter(|node| node.kind() == "identifier") else {
        return Ok(None);
    };
    let binding = javascript_scopes::read_binding(builder, receiver)?.nearest;
    Ok(match binding {
        NearestBinding::Module if constructor => Some(JavascriptMemberReceiver::Constructor(
            builder.context.owned_text(receiver)?,
        )),
        NearestBinding::Module => None,
        NearestBinding::Imported => Some(JavascriptMemberReceiver::LocalImport),
        _ => Some(JavascriptMemberReceiver::Shadowed),
    })
}

fn receiver_root(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_RECEIVER_DEPTH {
        if !matches!(node.kind(), "member_expression" | "subscript_expression") {
            return Some(node);
        }
        node = node.child_by_field_name("object")?;
    }
    None
}
