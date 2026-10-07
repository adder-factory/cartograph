//! Callable declarations shadow value constructors in their containing scope.

use super::{
    BindingType, ExtractError, ExtractionContext, NamedBind, ScopeKind, SyntaxIndex, Visit,
    node_text,
};

pub(super) fn assignment<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let name = visit
        .node
        .child_by_field_name("left")
        .or_else(|| super::named_children(visit.node).next())
        .map(assignment_target);
    if let Some((receiver, member)) = name.and_then(super::expressions::member_parts) {
        return index.assignment_member(
            context,
            super::super::MemberParts {
                visit,
                receiver: Some(receiver),
                member: Some(member),
            },
        );
    }
    if let Some(name) = name {
        invalidate_visible(index, context, (visit.scope, name))?;
    }
    super::bind_unknown(index, context, (visit, name))
}

pub(in super::super) struct ValueWrite<'tree> {
    scope: usize,
    name: super::Node<'tree>,
}

pub(in super::super) fn invalidate_visible<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    input: (usize, super::Node<'tree>),
) -> Result<(), ExtractError> {
    let (scope, name) = input;
    super::reserve_slot(context, &mut index.value_writes)?;
    index.value_writes.push(ValueWrite { scope, name });
    Ok(())
}

pub(in super::super) fn fence_values(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
) -> Result<(), ExtractError> {
    for write in std::mem::take(&mut index.value_writes) {
        context.ensure_active()?;
        let parent = index
            .scopes
            .get(&write.scope)
            .and_then(|scope| scope.parent);
        if let Some(parent) = parent {
            fence_visible(index, context, (parent, write.name))?;
        }
    }
    Ok(())
}

fn fence_visible(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (usize, super::Node<'_>),
) -> Result<(), ExtractError> {
    let (mut scope, name) = input;
    let name = super::binding_key(
        context,
        node_text(context, name).trim().trim_start_matches('$'),
    )?;
    for _ in 0..=super::super::super::MAX_AST_DEPTH {
        context.ensure_active()?;
        let Some(entry) = index.scopes.get_mut(&scope) else {
            break;
        };
        if let Some(binding) = entry.bindings.get_mut(name.as_ref()) {
            if !matches!(
                binding.kind,
                BindingType::Explicit {
                    origin: super::super::TypeOrigin::Annotation,
                    ..
                }
            ) {
                binding.kind = BindingType::Unknown;
            }
            break;
        }
        let Some(parent) = entry.parent else {
            break;
        };
        scope = parent;
    }
    Ok(())
}

fn assignment_target(mut node: super::Node<'_>) -> super::Node<'_> {
    for _ in 0..super::MAX_TYPE_WRAPPERS {
        if !matches!(node.kind(), "expression" | "directly_assignable_expression") {
            break;
        }
        let mut children = super::named_children(node);
        let Some(child) = children.next() else {
            break;
        };
        if children.next().is_some() {
            break;
        }
        node = child;
    }
    node
}

pub(super) fn collect(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    if visit.node.kind() == "function_signature" {
        let scope = if visit
            .node
            .parent()
            .is_some_and(|node| node.kind() == "lambda_expression")
        {
            index
                .scopes
                .get(&visit.scope)
                .and_then(|scope| scope.parent)
                .unwrap_or(visit.scope)
        } else {
            visit.scope
        };
        return bind_callable(
            index,
            context,
            (scope, visit.node.child_by_field_name("name")),
        );
    }
    if visit.node.kind() == "function_expression" {
        return bind_callable(
            index,
            context,
            (visit.scope, visit.node.child_by_field_name("name")),
        );
    }
    if !matches!(
        visit.node.kind(),
        "method_declaration" | "method_definition" | "function_declaration" | "function_definition"
    ) {
        return Ok(());
    }
    let Some(parent) = index
        .scopes
        .get(&visit.scope)
        .and_then(|scope| scope.parent)
    else {
        return Ok(());
    };
    bind_callable(
        index,
        context,
        (parent, visit.node.child_by_field_name("name")),
    )
}

fn bind_callable(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (usize, Option<super::Node<'_>>),
) -> Result<(), ExtractError> {
    let (scope, name) = input;
    let Some(name) = name else {
        return Ok(());
    };
    let kind = if index
        .scopes
        .get(&scope)
        .is_some_and(|scope| scope.kind == ScopeKind::Class)
    {
        BindingType::Method
    } else {
        BindingType::Unknown
    };
    index.bind_name(
        context,
        NamedBind {
            scope,
            name: node_text(context, name),
            kind,
            start: 0,
        },
    )
}
