//! Clojure and Common Lisp structural extraction.
//!
//! Both grammars parse every form as a plain list, so declarations, imports,
//! and calls are recognized from the list head symbol. A head that is not a
//! declaration, import, binding, or special form is an invocation of that
//! symbol. Quoted data is never evaluated, so it never declares or calls.

mod clojure;
mod common_lisp;

use cartograph_domain::SourceLanguage;
use tree_sitter::{Node, TreeCursor};

use crate::ExtractError;

use super::{ExtractionBuilder, syntax::named_children};

/// Index of the first form after a list's head: a definition's name, a binding
/// form's bindings, or an import form's first argument.
const AFTER_HEAD: usize = 1;
/// Index of the first form after a list's head and name: a definition's body.
const AFTER_NAME: usize = 2;
/// Child nodes of a list that are not among its forms.
const NON_FORM_KINDS: &[&str] = &[
    "comment",
    "block_comment",
    "dis_expr",
    "meta_lit",
    "old_meta_lit",
];
/// Reader forms whose contents are evaluated inside a syntax-quoted template.
const UNQUOTE_KINDS: &[&str] = &["unquoting_lit", "unquote_splicing_lit"];
/// Quoting level directly inside the template being visited; an unquote at
/// this level is evaluated when the template is.
const TEMPLATE_LEVEL: usize = 1;

/// Dispatch one Clojure or Common Lisp node; `false` lets the walker descend.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match (builder.context.snapshot.language(), node.kind()) {
        // Quoted data and discarded forms are never evaluated, so nothing in
        // them declares or calls anything.
        (_, "quoting_lit" | "dis_expr") => Ok(true),
        // Only metadata on a collection literal is evaluated; metadata on an
        // invocation or a symbol (type hints, flags) is never walked as code.
        (SourceLanguage::Clojure, "meta_lit" | "old_meta_lit") => Ok(!annotates_collection(node)),
        (_, "syn_quoting_lit") => {
            visit_syntax_quote(builder, node, depth)?;
            Ok(true)
        }
        // `#(head args...)` is an anonymous function whose body is one list
        // form, so its head is classified exactly like a list's.
        (SourceLanguage::Clojure, "list_lit" | "anon_fn_lit") => {
            clojure::visit_list(builder, node, depth)
        }
        (SourceLanguage::CommonLisp, "list_lit") => common_lisp::visit_list(builder, node, depth),
        (SourceLanguage::CommonLisp, "defun") => common_lisp::visit_defun(builder, node, depth),
        _ => Ok(false),
    }
}

/// Whether reader metadata is attached to a vector, map, or set literal, whose
/// metadata the compiler evaluates.
fn annotates_collection(metadata: Node<'_>) -> bool {
    metadata
        .parent()
        .is_some_and(|annotated| matches!(annotated.kind(), "vec_lit" | "map_lit" | "set_lit"))
}

/// Direct child forms of a list, without comments, discarded forms, or the
/// reader metadata that may precede a list's head.
fn form_children(node: Node<'_>) -> Vec<Node<'_>> {
    named_children(node)
        .filter(|child| !NON_FORM_KINDS.contains(&child.kind()))
        .collect()
}

/// A syntax-quoted template is data except for the unquoted forms that belong
/// to it, which the macro evaluates; only those are visited.
///
/// Each nested template opens a quoting level and each unquote closes one, so
/// an unquote belongs to this template only when it closes the outermost
/// level: `,(ghost)` inside an inner template is data until the inner
/// template itself is evaluated, while `,,x` evaluates `x` here. The cursor
/// walk tracks the level without allocating and never enters discarded forms
/// or the forms an evaluated unquote already visits.
fn visit_syntax_quote(
    builder: &mut ExtractionBuilder<'_, '_>,
    template: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let child_depth = depth.saturating_add(1);
    let mut cursor = template.walk();
    let mut level = TEMPLATE_LEVEL;
    if !cursor.goto_first_child() {
        return Ok(());
    }
    loop {
        builder.context.ensure_active()?;
        let node = cursor.node();
        let evaluated = level == TEMPLATE_LEVEL && UNQUOTE_KINDS.contains(&node.kind());
        if evaluated {
            for form in named_children(node) {
                builder.visit(form, child_depth)?;
            }
        }
        if !evaluated && node.kind() != "dis_expr" && cursor.goto_first_child() {
            level = entered_level(level, node.kind());
            continue;
        }
        if !advance_template_cursor(&mut cursor, template, &mut level) {
            return Ok(());
        }
    }
}

/// Advance to the next sibling, restoring the level of finished parents;
/// `false` when the cursor has left the template's contents.
fn advance_template_cursor(
    cursor: &mut TreeCursor<'_>,
    template: Node<'_>,
    level: &mut usize,
) -> bool {
    while !cursor.goto_next_sibling() {
        if !cursor.goto_parent() || cursor.node().id() == template.id() {
            return false;
        }
        *level = left_level(*level, cursor.node().kind());
    }
    true
}

/// The quoting level inside a node of kind `kind` entered at `level`.
fn entered_level(level: usize, kind: &str) -> usize {
    if kind == "syn_quoting_lit" {
        level.saturating_add(1)
    } else if UNQUOTE_KINDS.contains(&kind) {
        level.saturating_sub(1)
    } else {
        level
    }
}

/// The quoting level around a node of kind `kind` whose inside was at `level`.
fn left_level(level: usize, kind: &str) -> usize {
    if kind == "syn_quoting_lit" {
        level.saturating_sub(1)
    } else if UNQUOTE_KINDS.contains(&kind) {
        level.saturating_add(1)
    } else {
        level
    }
}
