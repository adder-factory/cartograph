//! Allocation-free preorder traversal for framework syntax signals.

use tree_sitter::{Node, TreeCursor};

pub(crate) struct SyntaxNodes<'tree> {
    cursor: TreeCursor<'tree>,
    finished: bool,
}

impl<'tree> SyntaxNodes<'tree> {
    pub(crate) fn new(root: Node<'tree>) -> Self {
        Self {
            cursor: root.walk(),
            finished: false,
        }
    }

    fn advance(&mut self) {
        if self.cursor.goto_first_child() {
            return;
        }
        while !self.cursor.goto_next_sibling() {
            if !self.cursor.goto_parent() {
                self.finished = true;
                return;
            }
        }
    }
}

impl<'tree> Iterator for SyntaxNodes<'tree> {
    type Item = Node<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let node = self.cursor.node();
        self.advance();
        Some(node)
    }
}
