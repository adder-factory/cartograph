use super::{
    Bind, BindingType, ExtractError, ExtractionContext, MemberParts, MemberSite, Node, ScopeKind,
    SyntaxIndex, Visit, explicit, initializer, named_children,
};

pub(super) fn scope_kind(node: Node<'_>) -> Option<ScopeKind> {
    match node.kind() {
        "type_spec"
            if node
                .child_by_field_name("type")
                .is_some_and(|node| matches!(node.kind(), "struct_type" | "interface_type")) =>
        {
            Some(ScopeKind::Class)
        }
        "function_declaration" | "method_declaration" | "func_literal" => Some(ScopeKind::Callable),
        "block"
        | "if_statement"
        | "for_statement"
        | "expression_switch_statement"
        | "type_switch_statement"
        | "select_statement"
        | "expression_case"
        | "type_case"
        | "communication_case" => Some(ScopeKind::Block),
        _ => None,
    }
}

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "parameter_declaration"
        | "variadic_parameter_declaration"
        | "field_declaration"
        | "var_spec" => typed_names(index, context, visit),
        "type_switch_statement" => type_switch_alias(index, context, visit),
        "short_var_declaration" | "assignment_statement" | "range_clause" | "receive_statement" => {
            local_names(index, context, visit)
        }
        "selector_expression" => index.member_sites(
            context,
            MemberParts {
                visit,
                receiver: visit.node.child_by_field_name("operand"),
                member: visit.node.child_by_field_name("field"),
            },
        ),
        "keyed_element" => literal_member(index, context, visit),
        "function_declaration"
        | "method_declaration"
        | "type_parameter_declaration"
        | "const_spec"
        | "type_spec"
        | "type_alias" => unknown_names(index, context, visit),
        _ => Ok(()),
    }
}

fn type_node(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..8 {
        match node.kind() {
            "type_identifier" | "qualified_type" => return Some(node),
            "pointer_type" => node = named_children(node).next()?,
            _ => return None,
        }
    }
    None
}

fn typed_names(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let annotation = (visit.node.kind() != "variadic_parameter_declaration")
        .then(|| visit.node.child_by_field_name("type").and_then(type_node))
        .flatten();
    let start = if visit.node.kind() == "var_spec" {
        visit.node.end_byte()
    } else {
        0
    };
    let mut cursor = visit.node.walk();
    for name in visit.node.children_by_field_name("name", &mut cursor) {
        context.ensure_active()?;
        let kind = explicit(context, annotation, visit.scope)?;
        index.bind(
            context,
            Bind {
                scope: visit.scope,
                name,
                kind,
                start,
            },
        )?;
    }
    Ok(())
}

fn type_switch_alias(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let Some(alias) = visit.node.child_by_field_name("alias") else {
        return Ok(());
    };
    if alias.kind() == "identifier" {
        return index.bind(
            context,
            Bind {
                scope: visit.scope,
                name: alias,
                kind: BindingType::Unknown,
                start: 0,
            },
        );
    }
    for name in named_children(alias) {
        context.ensure_active()?;
        index.bind(
            context,
            Bind {
                scope: visit.scope,
                name,
                kind: BindingType::Unknown,
                start: 0,
            },
        )?;
    }
    Ok(())
}

fn local_names(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let Some(left) = visit.node.child_by_field_name("left") else {
        return Ok(());
    };
    let mut names = named_children(left);
    let first = names.next();
    let sole = names.next().is_none();
    let constructed = (visit.node.kind() == "short_var_declaration" && sole)
        .then(|| {
            visit
                .node
                .child_by_field_name("right")
                .and_then(|node| named_children(node).next())
                .and_then(literal_type)
        })
        .flatten();
    for name in named_children(left) {
        context.ensure_active()?;
        let kind = initializer(
            context,
            constructed.filter(|_| Some(name) == first),
            visit.scope,
        )?;
        index.bind(
            context,
            Bind {
                scope: visit.scope,
                name,
                kind,
                start: visit.node.end_byte(),
            },
        )?;
    }
    Ok(())
}

fn literal_type(mut node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "unary_expression" {
        if node.child(0)?.kind() != "&" {
            return None;
        }
        node = node.child_by_field_name("operand")?;
    }
    (node.kind() == "composite_literal")
        .then(|| node.child_by_field_name("type").and_then(type_node))
        .flatten()
}

fn unknown_names(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    if index.declarations.contains_key(&visit.node.start_byte()) {
        return Ok(());
    }
    let scope = if matches!(
        visit.node.kind(),
        "function_declaration" | "method_declaration"
    ) {
        index
            .scopes
            .get(&visit.scope)
            .and_then(|scope| scope.parent)
            .unwrap_or(visit.scope)
    } else {
        visit.scope
    };
    let mut cursor = visit.node.walk();
    for name in visit.node.children_by_field_name("name", &mut cursor) {
        context.ensure_active()?;
        index.bind(
            context,
            Bind {
                scope,
                name,
                kind: BindingType::Unknown,
                start: 0,
            },
        )?;
    }
    Ok(())
}

fn literal_member<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let Some(member) = visit
        .node
        .child_by_field_name("key")
        .and_then(|node| named_children(node).next())
    else {
        return Ok(());
    };
    let Some(receiver) = visit
        .node
        .parent()
        .and_then(|node| node.parent())
        .filter(|node| node.kind() == "composite_literal")
        .and_then(|node| node.child_by_field_name("type"))
        .and_then(type_node)
    else {
        return Ok(());
    };
    index.member(
        context,
        MemberSite {
            receiver,
            member,
            scope: visit.scope,
            start: member.start_byte(),
            end: member.end_byte(),
            literal: true,
        },
    )
}
