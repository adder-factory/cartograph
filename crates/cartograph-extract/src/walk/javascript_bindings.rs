//! Names bound by a JavaScript-family binding target.
//!
//! A parameter list, a `catch` parameter, a declared loop variable, or an
//! assignment target binds the identifiers in its binding positions only:
//! default values (`x = DEFAULT`), computed keys, type annotations, and
//! member or subscript targets (`cache.x = ..`) bind nothing. Every node the
//! scan queues is charged to a caller-owned budget before it is enumerated
//! further, so callers bound their total work and treat exhaustion
//! conservatively.

use tree_sitter::Node;

use super::syntax::named_children;

/// Result of scanning one binding target for a name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BindingMatch {
    /// A bound name satisfied the predicate.
    Found,
    /// No bound name satisfied the predicate.
    Absent,
    /// The node budget ran out before the scan finished.
    Exhausted,
}

/// Visit the names bound by `target`, stopping at the first one `matches`
/// accepts. Every queued node spends one unit of `budget`.
pub(super) fn scan_bound_names<'tree>(
    target: Node<'tree>,
    budget: &mut usize,
    mut matches: impl FnMut(Node<'tree>) -> bool,
) -> BindingMatch {
    if !charge(budget) {
        return BindingMatch::Exhausted;
    }
    let mut pending = vec![target];
    while let Some(node) = pending.pop() {
        let children = match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                if matches(node) {
                    return BindingMatch::Found;
                }
                continue;
            }
            "member_expression"
            | "subscript_expression"
            | "type_annotation"
            | "computed_property_name" => continue,
            _ => binding_children(node),
        };
        for child in children {
            if !charge(budget) {
                return BindingMatch::Exhausted;
            }
            pending.push(child);
        }
    }
    BindingMatch::Absent
}

/// The children of a binding node that are themselves binding positions.
pub(super) fn binding_children(node: Node<'_>) -> Box<dyn Iterator<Item = Node<'_>> + '_> {
    let field = match node.kind() {
        "assignment_pattern" | "object_assignment_pattern" => Some("left"),
        "pair_pattern" => Some("value"),
        "required_parameter" | "optional_parameter" => {
            return Box::new(
                node.child_by_field_name("pattern")
                    .or_else(|| node.child_by_field_name("name"))
                    .into_iter(),
            );
        }
        _ => None,
    };
    match field {
        Some(field) => Box::new(node.child_by_field_name(field).into_iter()),
        None => Box::new(named_children(node)),
    }
}

/// Spend one unit of budget, reporting whether any was left.
fn charge(budget: &mut usize) -> bool {
    match budget.checked_sub(1) {
        Some(remaining) => {
            *budget = remaining;
            true
        }
        None => false,
    }
}
