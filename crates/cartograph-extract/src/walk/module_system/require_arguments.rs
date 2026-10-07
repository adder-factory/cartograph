//! A comment is syntax trivia, never the `CommonJS` module argument.

use tree_sitter::Node;

// This pure shape lookup remains constant-bounded between walker polls.
const MAX_ARGUMENT_NODES: usize = 64;

pub(super) fn single_value(arguments: Node<'_>) -> Option<Node<'_>> {
    if arguments.named_child_count() > MAX_ARGUMENT_NODES {
        return None;
    }
    let mut cursor = arguments.walk();
    let mut values = arguments
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra());
    let value = values.next()?;
    values.next().is_none().then_some(value)
}
