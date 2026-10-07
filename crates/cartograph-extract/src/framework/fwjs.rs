//! Bounded syntax-backed framework detail for JavaScript and `NeuG`.
mod angular;
mod cli;
mod commonjs;
mod express;
pub(super) mod file_routes;
mod lexical;
mod neug;

use crate::{ExtractError, framework::FrameworkBuilder};
use cartograph_domain::SourceLanguage;
use tree_sitter::Node;

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    masked_source: &str,
) -> Result<(), ExtractError> {
    if builder.source().is_empty() {
        return Ok(());
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(());
    };
    let javascript = matches!(
        builder.language(),
        SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    );
    if javascript {
        let explicit_framework = super::javascript_framework_route_hint(masked_source);
        let commonjs = commonjs::Index::build(builder)?;
        let lexical = lexical::Index::build(builder)?;
        angular::scan(builder, root, &lexical)?;
        walk(builder, root, |builder, node| {
            if node.kind() == "call_expression" {
                commonjs.capture(builder, node)?;
                cli::scan_call(builder, node)?;
                if !explicit_framework {
                    express::scan_call(builder, node, &lexical)?;
                }
            }
            Ok(())
        })?;
    } else if builder.language() == SourceLanguage::Python {
        neug::scan(builder, root)?;
    }
    Ok(())
}

/// Cursor traversal has constant retained state; every AST visit charges the
/// existing bridge work budget and observes cancellation.
fn walk<'source, 'tree>(
    builder: &mut FrameworkBuilder<'source, '_>,
    root: Node<'tree>,
    mut visit: impl FnMut(&mut FrameworkBuilder<'source, '_>, Node<'tree>) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    let mut cursor = root.walk();
    loop {
        builder.bridge.charge_work(1)?;
        visit(builder, cursor.node())?;
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
    }
}

fn text<'source>(builder: &FrameworkBuilder<'source, '_>, node: Node<'_>) -> &'source str {
    builder.source().get(node.byte_range()).unwrap_or_default()
}

fn literal<'source>(
    builder: &FrameworkBuilder<'source, '_>,
    node: Node<'_>,
) -> Option<&'source str> {
    if !matches!(node.kind(), "string" | "template_string") {
        return None;
    }
    let raw = text(builder, node);
    let quote = raw.chars().next()?;
    let value = raw.strip_circumfix(quote, quote)?;
    (!value.contains(['\\', '\n', '\r']) && !value.contains("${")).then_some(value)
}

fn first_argument(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("arguments")?.named_child(0)
}

/// The textual registration marker must also be a native call callee; quoted
/// source and suffixes inside another receiver cannot carry registrations.
pub(crate) fn member_call_is_syntax(
    builder: &FrameworkBuilder<'_, '_>,
    (start, end): (usize, usize),
) -> bool {
    builder
        .syntax_root()
        .and_then(|root| root.descendant_for_byte_range(start, end))
        .is_some_and(|node| {
            node.kind() == "member_expression"
                && node.start_byte() == start
                && node.end_byte() == end
                && node.parent().is_some_and(|parent| {
                    parent.kind() == "call_expression"
                        && parent.child_by_field_name("function") == Some(node)
                })
        })
}
