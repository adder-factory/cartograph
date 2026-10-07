use super::{
    ExtractError, ExtractionContext, MAX_CHAIN_CALLS, MAX_TYPE_WRAPPERS, MemberSite, Node,
    ReceiverTypes, SourceLanguage, TypeQuery, identifier, named_children, node_text, prefixed_type,
    type_node,
};

pub(super) fn constructor(language: SourceLanguage, node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "object_creation_expression" => node.child_by_field_name("type").and_then(type_node),
        "new_expression" => node
            .child_by_field_name("constructor")
            .or_else(|| node.child_by_field_name("type"))
            .or_else(|| named_children(node).next())
            .and_then(type_node),
        "instance_expression" => named_children(node).next().and_then(type_node),
        "application_expression" if language == SourceLanguage::Ocaml => node
            .child_by_field_name("function")
            .filter(|function| function.kind() == "new_expression")
            .and_then(|function| named_children(function).next())
            .and_then(type_node),
        "call_expression" if language == SourceLanguage::Kotlin => kotlin_constructor(node),
        "call_expression" if language == SourceLanguage::Swift => named_children(node)
            .next()
            .filter(|node| matches!(node.kind(), "identifier" | "simple_identifier")),
        _ => None,
    }
}

fn kotlin_constructor(node: Node<'_>) -> Option<Node<'_>> {
    let mut children = named_children(node);
    let name = children.next()?;
    let suffix = children
        .next()
        .filter(|node| node.kind() == "call_suffix")?;
    if children.next().is_some() {
        return None;
    }
    let mut arguments = named_children(suffix);
    let values = arguments
        .next()
        .filter(|node| node.kind() == "value_arguments")?;
    if arguments.next().is_some() || values.named_child_count() != 0 {
        return None;
    }
    matches!(name.kind(), "identifier" | "simple_identifier").then_some(name)
}

pub(super) fn expression_type(
    types: &ReceiverTypes<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (MemberSite<'_>, usize),
) -> Result<Option<String>, ExtractError> {
    let (site, depth) = input;
    context.ensure_active()?;
    if depth > MAX_CHAIN_CALLS || !super::cpp::permits_access(types, context, (site, depth))? {
        return Ok(None);
    }
    let node = unwrap(site.receiver);
    let site = MemberSite {
        receiver: node,
        ..site
    };
    if let Some(name) = variable_name(context, node) {
        let binding = types.find_binding(
            context,
            TypeQuery {
                scope: site.scope,
                name,
                class_bindings: true,
                position: None,
            },
        )?;
        return match binding {
            Some(binding) if binding.start <= site.start => types.bound_type(context, binding),
            Some(_) | None => Ok(None),
        };
    }
    if let Some(name) = constructor_type(context, node) {
        return super::constructors::explicit_type(
            types,
            context,
            (
                TypeQuery {
                    name: node_text(context, name),
                    scope: site.scope,
                    class_bindings: false,
                    position: Some(node.start_byte()),
                },
                true,
            ),
        );
    }
    if let Some((receiver, member)) = call_member(node) {
        return returned_type(types, context, (site, receiver, member, depth));
    }
    field_type(types, context, (site, depth))
}

fn variable_name<'source>(
    context: &ExtractionContext<'source, '_>,
    node: Node<'_>,
) -> Option<&'source str> {
    if !matches!(
        node.kind(),
        "identifier" | "simple_identifier" | "value_name" | "value_path" | "variable"
    ) {
        return None;
    }
    let name = node_text(context, node).trim_start_matches('$');
    identifier(name).then_some(name)
}

fn constructor_type<'tree>(
    context: &ExtractionContext<'_, '_>,
    node: Node<'tree>,
) -> Option<Node<'tree>> {
    if node.kind() == "call" && context.snapshot.language() == SourceLanguage::Ruby {
        return (node_text(context, node.child_by_field_name("method")?) == "new")
            .then(|| node.child_by_field_name("receiver"))
            .flatten();
    }
    constructor(context.snapshot.language(), node)
}

fn returned_type(
    types: &ReceiverTypes<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (MemberSite<'_>, Node<'_>, Node<'_>, usize),
) -> Result<Option<String>, ExtractError> {
    let (site, receiver, member, depth) = input;
    let name = node_text(context, member);
    if !identifier(name) {
        return Ok(None);
    }
    let nested = MemberSite {
        receiver,
        member,
        ..site
    };
    let marker = expression_type(types, context, (nested, depth.saturating_add(1)))?;
    let marker = if let Some(marker) = marker {
        Some(marker)
    } else {
        let marker = types.explicit_type(
            context,
            TypeQuery {
                name: node_text(context, receiver),
                scope: site.scope,
                class_bindings: false,
                position: Some(receiver.start_byte()),
            },
        )?;
        marker
            .map(|marker| prefixed_type(context, ("static::", &marker)))
            .transpose()?
    };
    let Some(marker) = marker.filter(|marker| marker != "?" && marker != "static::?") else {
        return Ok(None);
    };
    if marker.split('|').count() > MAX_CHAIN_CALLS {
        return Ok(None);
    }
    let suffix = prefixed_type(context, ("|", name))?;
    prefixed_type(context, (&marker, &suffix)).map(Some)
}

fn field_type(
    types: &ReceiverTypes<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (MemberSite<'_>, usize),
) -> Result<Option<String>, ExtractError> {
    let (site, _) = input;
    if matches!(site.receiver.kind(), "this" | "this_expression")
        || node_text(context, site.receiver) == "self"
    {
        return types.this_type(context, site.scope);
    }
    let Some(binding) = field_binding(types, context, input)? else {
        return Ok(None);
    };
    types.bound_type(context, binding)
}

pub(super) fn field_binding<'index>(
    types: &ReceiverTypes<'index>,
    context: &mut ExtractionContext<'_, '_>,
    input: (MemberSite<'_>, usize),
) -> Result<Option<&'index super::super::Binding>, ExtractError> {
    let (site, depth) = input;
    let Some((receiver, member)) = member_parts(site.receiver) else {
        return Ok(None);
    };
    let parent = expression_type(
        types,
        context,
        (
            MemberSite {
                receiver,
                member,
                ..site
            },
            depth.saturating_add(1),
        ),
    )?;
    let Some(scope) = parent
        .as_deref()
        .and_then(super::constructors::class_id)
        .and_then(|id| types.class_scopes.get(id))
        .and_then(|scope| types.scopes.get(scope))
    else {
        return Ok(None);
    };
    Ok(scope.bindings.get(node_text(context, member)))
}

pub(super) fn member_parts(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    match node.kind() {
        "navigation_expression" => {
            let receiver = named_children(node).next()?;
            let suffix = named_children(node).last()?;
            Some((receiver, named_children(suffix).last()?))
        }
        "member_expression" | "field_access" | "member_access_expression" | "field_expression" => {
            Some((
                node.child_by_field_name("object")
                    .or_else(|| node.child_by_field_name("expression"))
                    .or_else(|| node.child_by_field_name("argument"))
                    .or_else(|| node.child_by_field_name("value"))?,
                node.child_by_field_name("property")
                    .or_else(|| node.child_by_field_name("field"))
                    .or_else(|| node.child_by_field_name("name"))?,
            ))
        }
        "call" => Some((
            node.child_by_field_name("receiver")?,
            node.child_by_field_name("method")?,
        )),
        "method_invocation" => Some((
            node.child_by_field_name("object")?,
            node.child_by_field_name("name")
                .or_else(|| node.child_by_field_name("method"))?,
        )),
        _ => None,
    }
}

fn call_member(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    if matches!(node.kind(), "call" | "method_invocation") {
        return member_parts(node);
    }
    if !matches!(node.kind(), "call_expression" | "invocation_expression") {
        return None;
    }
    let target = node
        .child_by_field_name("function")
        .or_else(|| named_children(node).next())?;
    member_parts(unwrap(target))
}

fn unwrap(mut node: Node<'_>) -> Node<'_> {
    for _ in 0..MAX_TYPE_WRAPPERS {
        if !matches!(
            node.kind(),
            "expression" | "parenthesized_expression" | "value_path"
        ) {
            break;
        }
        let mut children = named_children(node);
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
