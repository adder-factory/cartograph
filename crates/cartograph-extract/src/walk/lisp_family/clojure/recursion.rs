//! Direct defn body/arity calls with an unshadowed owner name prove recursion.

use tree_sitter::Node;

use crate::{CallScopeKind, ExtractError, ExtractedCallScopeSite};

use super::super::super::{
    ExtractionBuilder,
    syntax::{named_children, span_for},
};

const MAX_PARAMETER_NODES: usize = 128;
const MAX_HEADER_NODES: usize = 16;

fn form(node: Node<'_>, position: usize) -> Option<Node<'_>> {
    named_children(node)
        .take(MAX_HEADER_NODES)
        .filter(|child| !super::super::NON_FORM_KINDS.contains(&child.kind()))
        .nth(position)
}

fn definition(call: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let parent = call.parent().filter(|node| node.kind() == "list_lit")?;
    if let Some(parameters) = form(parent, 0).filter(|node| node.kind() == "vec_lit") {
        return Some((parent.parent()?, parameters));
    }
    let parameters = form(parent, super::AFTER_NAME).filter(|node| node.kind() == "vec_lit")?;
    Some((parent, parameters))
}

fn parameters_are_clean(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    let mut cursor = parameters.walk();
    let mut depth = 0_usize;
    for _ in 0..MAX_PARAMETER_NODES {
        builder.context.ensure_active()?;
        let node = cursor.node();
        if super::symbol(builder.context.snapshot.source(), node) == Some(name) {
            return Ok(false);
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        while depth > 0 {
            if cursor.goto_next_sibling() {
                break;
            }
            cursor.goto_parent();
            depth -= 1;
        }
        if depth == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn capture(
    builder: &mut ExtractionBuilder<'_, '_>,
    (call, head, name): (Node<'_>, Node<'_>, &str),
) -> Result<(), ExtractError> {
    let Some((definition, parameters)) = definition(call) else {
        return Ok(());
    };
    let source = builder.context.snapshot.source();
    let operator = form(definition, 0).and_then(|node| super::symbol(source, node));
    let declared = form(definition, super::AFTER_HEAD).and_then(|node| super::symbol(source, node));
    if !matches!(operator, Some("defn" | "defn-"))
        || declared != Some(name)
        || !parameters_are_clean(builder, parameters, name)?
    {
        return Ok(());
    }
    let Some(owner) = builder.owners.last().cloned() else {
        return Ok(());
    };
    let site = ExtractedCallScopeSite {
        owner,
        span: span_for(head)?,
        kind: CallScopeKind::DirectCallable,
    };
    builder.context.budget.reserve_fact(
        crate::budget::call_scope_site_budget_bytes(&site),
        [site.owner.as_str()],
    )?;
    builder
        .facts
        .call_scope_sites
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.facts.call_scope_sites.push(site);
    Ok(())
}
