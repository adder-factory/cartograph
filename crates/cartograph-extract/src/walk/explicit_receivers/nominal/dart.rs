use super::{
    ExtractError, ExtractionContext, MemberSite, Node, SyntaxIndex, Visit, bind_kind, bind_typed,
    initializer, named_children,
};

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "initialized_variable_definition" => variable(index, context, visit),
        "initialized_identifier" => field(index, context, visit),
        "function_body" => parameters(index, context, visit),
        "selector" => call(index, context, visit),
        _ => Ok(()),
    }
}

fn annotation(node: Node<'_>) -> Option<Node<'_>> {
    // Separate type arguments denote a generic container, not its element.
    if named_children(node).any(|node| node.kind() == "type_arguments") {
        return None;
    }
    named_children(node).find(|node| matches!(node.kind(), "type_identifier" | "nullable_type"))
}

fn variable(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let name = visit.node.child_by_field_name("name");
    if let Some(annotation) = annotation(visit.node) {
        return bind_typed(index, context, (visit, name, Some(annotation)));
    }
    let mut cursor = visit.node.walk();
    let mut values = visit.node.children_by_field_name("value", &mut cursor);
    let receiver = values.next();
    let arguments = values.next();
    let constructor = receiver
        .filter(|node| node.kind() == "identifier")
        .filter(|_| arguments.is_some_and(argument_selector) && values.next().is_none());
    let Some(name) = name else {
        return Ok(());
    };
    let kind = initializer(context, constructor, visit.scope)?;
    bind_kind(index, context, (visit, name, kind))
}

fn field(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let annotation = visit
        .node
        .parent()
        .and_then(|node| node.parent())
        .and_then(annotation);
    bind_typed(
        index,
        context,
        (visit, named_children(visit.node).next(), annotation),
    )
}

fn parameters(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let Some(signature) = visit.node.prev_named_sibling() else {
        return Ok(());
    };
    let signature = named_children(signature)
        .find(|node| node.kind() == "function_signature")
        .unwrap_or(signature);
    let Some(parameters) =
        named_children(signature).find(|node| node.kind() == "formal_parameter_list")
    else {
        return Ok(());
    };
    for parameter in named_children(parameters) {
        context.ensure_active()?;
        if parameter.kind() != "formal_parameter" {
            index.fence(context, visit.scope)?;
            continue;
        }
        let name = parameter.child_by_field_name("name");
        let kind = super::explicit(
            context,
            annotation(parameter).and_then(super::type_node),
            visit.scope,
        )?;
        if let Some(name) = name {
            bind_kind(
                index,
                context,
                (
                    Visit {
                        node: parameter,
                        ..visit
                    },
                    name,
                    kind,
                ),
            )?;
        }
    }
    Ok(())
}

fn argument_selector(node: Node<'_>) -> bool {
    node.kind() == "selector"
        && named_children(node)
            .next()
            .is_some_and(|node| node.kind() == "argument_part")
}

fn call<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let Some(selector) = named_children(visit.node).next().filter(|node| {
        matches!(
            node.kind(),
            "unconditional_assignable_selector" | "conditional_assignable_selector"
        )
    }) else {
        return Ok(());
    };
    let Some(member) = named_children(selector).next() else {
        return Ok(());
    };
    let Some(arguments) = visit
        .node
        .next_named_sibling()
        .filter(|node| argument_selector(*node))
        .and_then(|node| named_children(node).next())
        .and_then(|node| named_children(node).last())
    else {
        return Ok(());
    };
    let Some(previous) = visit.node.prev_named_sibling() else {
        return Ok(());
    };
    let (receiver, literal) = if argument_selector(previous) {
        (previous.prev_named_sibling(), true)
    } else {
        (Some(previous), false)
    };
    let Some(receiver) = receiver.filter(|node| node.kind() == "identifier") else {
        return Ok(());
    };
    index.member(
        context,
        MemberSite {
            receiver,
            member,
            scope: visit.scope,
            start: arguments.start_byte(),
            end: arguments.end_byte(),
            literal,
        },
    )
}
