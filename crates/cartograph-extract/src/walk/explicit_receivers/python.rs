use super::{
    Bind, BindingType, ExtractError, ExtractionContext, MemberParts, Node, ScopeKind, SyntaxIndex,
    Visit, explicit, initializer, named_children,
};

pub(super) fn scope_kind(node: Node<'_>) -> Option<ScopeKind> {
    match node.kind() {
        "class_definition" => Some(ScopeKind::Class),
        "function_definition" => Some(ScopeKind::Callable),
        "lambda"
        | "list_comprehension"
        | "set_comprehension"
        | "dictionary_comprehension"
        | "generator_expression" => Some(ScopeKind::Opaque),
        _ => None,
    }
}

pub(super) fn heritage_supported(
    context: &mut ExtractionContext<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(bases) = node.child_by_field_name("superclasses") else {
        return Ok(true);
    };
    for base in named_children(bases) {
        context.ensure_active()?;
        if !matches!(base.kind(), "identifier" | "attribute") {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    if visit.node.child_by_field_name("type_parameters").is_some() {
        index.fence(context, visit.scope)?;
    }
    if parameter_node(visit.node) {
        return parameter(index, context, visit);
    }
    match visit.node.kind() {
        "function_definition" => declare_function(index, context, visit),
        "assignment" => assignment(index, context, visit),
        "augmented_assignment" | "for_statement" | "for_in_clause" => bind_target(
            index,
            context,
            AssignmentBinding {
                visit,
                target: visit.node.child_by_field_name("left"),
                kind: BindingType::Unknown,
            },
        ),
        "global_statement" | "nonlocal_statement" | "delete_statement" => {
            bind_named_targets(index, context, visit)
        }
        "import_statement" | "import_from_statement" => imports(index, context, visit),
        "attribute" => index.member_sites(
            context,
            MemberParts {
                visit,
                receiver: visit.node.child_by_field_name("object"),
                member: visit.node.child_by_field_name("attribute"),
            },
        ),
        "as_pattern" => bind_target(
            index,
            context,
            AssignmentBinding {
                visit,
                target: visit.node.child_by_field_name("alias"),
                kind: BindingType::Unknown,
            },
        ),
        "with_statement"
        | "except_clause"
        | "match_statement"
        | "named_expression"
        | "wildcard_import"
        | "type_alias_statement" => index.fence(context, visit.scope),
        _ => Ok(()),
    }
}

fn parameter_node(node: Node<'_>) -> bool {
    match node.kind() {
        "typed_parameter" | "typed_default_parameter" | "default_parameter" => true,
        "identifier" | "list_splat_pattern" | "dictionary_splat_pattern" => node
            .parent()
            .is_some_and(|node| node.kind() == "parameters"),
        _ => false,
    }
}

fn declare_function(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let Some(scope) = index
        .scopes
        .get(&visit.scope)
        .and_then(|scope| scope.parent)
    else {
        return Ok(());
    };
    if let Some(name) = visit.node.child_by_field_name("name") {
        let kind = if index
            .scopes
            .get(&scope)
            .is_some_and(|scope| scope.kind == ScopeKind::Class)
        {
            BindingType::Method
        } else {
            BindingType::Unknown
        };
        index.bind(
            context,
            Bind {
                scope,
                name,
                kind,
                start: 0,
            },
        )?;
    }
    Ok(())
}

fn parameter<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    if let Some(splat) = parameter_splat(visit.node) {
        return bind_target(
            index,
            context,
            AssignmentBinding {
                visit,
                target: Some(splat),
                kind: BindingType::Unknown,
            },
        );
    }
    let name = if visit.node.kind() == "identifier" {
        Some(visit.node)
    } else {
        visit
            .node
            .child_by_field_name("name")
            .or_else(|| named_children(visit.node).find(|node| node.kind() == "identifier"))
    };
    let Some(name) = name else { return Ok(()) };
    let kind = match self_nominal(index, context, visit)? {
        Some(nominal) => BindingType::Receiver(nominal),
        None => explicit(
            context,
            visit.node.child_by_field_name("type"),
            index
                .scopes
                .get(&visit.scope)
                .and_then(|scope| scope.parent)
                .unwrap_or(visit.scope),
        )?,
    };
    index.bind(
        context,
        Bind {
            scope: visit.scope,
            name,
            kind,
            start: 0,
        },
    )
}

fn parameter_splat(node: Node<'_>) -> Option<Node<'_>> {
    let is_splat = |node: Node<'_>| {
        matches!(
            node.kind(),
            "list_splat_pattern" | "dictionary_splat_pattern"
        )
    };
    if is_splat(node) {
        Some(node)
    } else {
        named_children(node).find(|node| is_splat(*node))
    }
}

fn self_nominal(
    index: &SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<Option<super::SymbolId>, ExtractError> {
    let Some(callable) = self_callable(context, visit.node) else {
        return Ok(None);
    };
    if !ordinary_decorators(context, callable)? {
        return Ok(None);
    }
    Ok(index
        .scopes
        .get(&visit.scope)
        .and_then(|scope| scope.parent)
        .and_then(|scope| index.scopes.get(&scope))
        .filter(|scope| scope.kind == ScopeKind::Class)
        .and_then(|scope| scope.nominal.clone()))
}

fn self_callable<'tree>(
    context: &ExtractionContext<'_, '_>,
    node: Node<'tree>,
) -> Option<Node<'tree>> {
    if node.kind() != "identifier" || context.text(node) != "self" {
        return None;
    }
    let parameters = node.parent().filter(|node| node.kind() == "parameters")?;
    if named_children(parameters).next()? != node {
        return None;
    }
    parameters.parent()
}

fn ordinary_decorators(
    context: &mut ExtractionContext<'_, '_>,
    callable: Node<'_>,
) -> Result<bool, ExtractError> {
    if let Some(decorated) = callable
        .parent()
        .filter(|node| node.kind() == "decorated_definition")
    {
        for node in named_children(decorated).filter(|node| node.kind() == "decorator") {
            context.ensure_active()?;
            if context.text(node).trim() != "@property" {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn assignment<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let annotation = visit.node.child_by_field_name("type");
    let constructed = visit
        .node
        .child_by_field_name("right")
        .filter(|node| node.kind() == "call")
        .and_then(|node| node.child_by_field_name("function"));
    let kind = if annotation.is_some() {
        explicit(context, annotation, visit.scope)?
    } else {
        initializer(context, constructed, visit.scope)?
    };
    bind_target(
        index,
        context,
        AssignmentBinding {
            visit,
            target: visit.node.child_by_field_name("left"),
            kind,
        },
    )
}

struct AssignmentBinding<'tree> {
    visit: Visit<'tree>,
    target: Option<Node<'tree>>,
    kind: BindingType,
}

fn bind_target<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    target: AssignmentBinding<'tree>,
) -> Result<(), ExtractError> {
    context.ensure_active()?;
    if target.visit.depth > super::super::MAX_AST_DEPTH {
        return Err(ExtractError::NestingLimit);
    }
    let Some(name) = target.target else {
        return Ok(());
    };
    if name.kind() == "attribute" {
        return index.assignment_member(
            context,
            MemberParts {
                visit: target.visit,
                receiver: name.child_by_field_name("object"),
                member: name.child_by_field_name("attribute"),
            },
        );
    }
    if name.kind() == "identifier" {
        return index.bind(
            context,
            Bind {
                scope: target.visit.scope,
                name,
                kind: target.kind,
                start: target.visit.node.end_byte(),
            },
        );
    }
    if matches!(
        name.kind(),
        "pattern_list"
            | "tuple_pattern"
            | "list_pattern"
            | "list_splat_pattern"
            | "dictionary_splat_pattern"
    ) {
        for child in named_children(name) {
            context.ensure_active()?;
            bind_target(
                index,
                context,
                AssignmentBinding {
                    visit: Visit {
                        depth: target.visit.depth.saturating_add(1),
                        ..target.visit
                    },
                    target: Some(child),
                    kind: BindingType::Unknown,
                },
            )?;
        }
    }
    Ok(())
}

fn bind_named_targets<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    for name in named_children(visit.node) {
        context.ensure_active()?;
        bind_target(
            index,
            context,
            AssignmentBinding {
                visit,
                target: Some(name),
                kind: BindingType::Unknown,
            },
        )?;
    }
    Ok(())
}

fn imports(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let mut cursor = visit.node.walk();
    for imported in visit.node.children_by_field_name("name", &mut cursor) {
        context.ensure_active()?;
        let name = imported
            .child_by_field_name("alias")
            .or_else(|| named_children(imported).next());
        if let Some(name) = name {
            index.bind(
                context,
                Bind {
                    scope: visit.scope,
                    name,
                    kind: BindingType::Import,
                    start: visit.node.end_byte(),
                },
            )?;
        }
    }
    Ok(())
}
