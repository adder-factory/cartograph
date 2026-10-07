//! Preserve receiver syntax even when Dart's durable callee drops that receiver.

use super::{named_child_of_kind, previous_code_sibling};
use crate::CallScopeKind;
use tree_sitter::Node;

pub(in super::super) fn current_call(node: Node<'_>) -> Option<CallScopeKind> {
    if node.kind() != "selector" {
        return None;
    }
    let previous = previous_code_sibling(node)?;
    if previous.kind() == "identifier" {
        return Some(CallScopeKind::CurrentClass);
    }
    if previous.kind() != "selector"
        || named_child_of_kind(previous, "unconditional_assignable_selector").is_none()
    {
        return None;
    }
    previous_code_sibling(previous)
        .filter(|receiver| receiver.kind() == "this")
        .map(|_| CallScopeKind::CurrentInstance)
}
