use super::{
    BindingType, ExtractError, ExtractionContext, NamedBind, Node, SourceLanguage, SyntaxIndex,
    Visit, bind_typed, bind_unknown, explicit, initializer, member, named_children, node_text,
};

const MAX_SCRIPT_WRAPPERS: usize = 32;

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match context.snapshot.language() {
        SourceLanguage::Ruby => ruby(index, context, visit),
        SourceLanguage::Ocaml => ocaml(index, context, visit),
        SourceLanguage::Pascal => pascal(index, context, visit),
        SourceLanguage::PowerShell => powershell(index, context, visit),
        _ => Ok(()),
    }
}

fn ruby<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "assignment" => {
            let value = visit.node.child_by_field_name("right");
            let constructed = value
                .filter(|node| node.kind() == "call")
                .filter(|node| {
                    node.child_by_field_name("method")
                        .is_some_and(|node| node_text(context, node) == "new")
                })
                .and_then(|node| node.child_by_field_name("receiver"));
            let kind = initializer(context, constructed, visit.scope)?;
            let Some(name) = visit.node.child_by_field_name("left") else {
                return Ok(());
            };
            if matches!(
                name.kind(),
                "instance_variable" | "class_variable" | "global_variable" | "element_reference"
            ) {
                return Ok(());
            }
            super::bindings::invalidate_visible(index, context, (visit.scope, name))?;
            super::bind_kind(index, context, (visit, name, kind))
        }
        "call" => member(
            index,
            context,
            (
                visit,
                visit.node.child_by_field_name("receiver"),
                visit.node.child_by_field_name("method"),
            ),
        ),
        "method_parameters" | "block_parameters" => unknown_children(index, context, visit),
        "operator_assignment" | "for" | "in_clause" | "exception_variable" => {
            ruby_unknown(index, context, visit)
        }
        _ => Ok(()),
    }
}

fn ruby_unknown<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let name = visit
        .node
        .child_by_field_name("left")
        .or_else(|| visit.node.child_by_field_name("pattern"))
        .or_else(|| named_children(visit.node).next());
    if let Some(name) = name {
        super::bindings::invalidate_visible(index, context, (visit.scope, name))?;
    }
    bind_unknown(index, context, (visit, name))
}

fn unknown_children(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    for child in named_children(visit.node) {
        context.ensure_active()?;
        if !super::identifier(node_text(context, child)) {
            index.fence(context, visit.scope)?;
        }
        bind_unknown(index, context, (visit, Some(child)))?;
    }
    Ok(())
}

fn ocaml<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "let_binding" => super::initialized(
            index,
            context,
            (
                visit,
                visit.node.child_by_field_name("pattern"),
                visit.node.child_by_field_name("body"),
            ),
        ),
        "parameter" => bind_unknown(
            index,
            context,
            (visit, visit.node.child_by_field_name("pattern")),
        ),
        "method_invocation" => member(
            index,
            context,
            (
                visit,
                visit.node.child_by_field_name("object"),
                visit.node.child_by_field_name("method"),
            ),
        ),
        _ => Ok(()),
    }
}

fn pascal<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "declVar" | "declArg" => bind_typed(
            index,
            context,
            (
                visit,
                visit.node.child_by_field_name("name"),
                visit.node.child_by_field_name("type"),
            ),
        ),
        "exprCall" => {
            let Some(entity) = visit
                .node
                .child_by_field_name("entity")
                .filter(|node| node.kind() == "exprDot")
            else {
                return Ok(());
            };
            member(
                index,
                context,
                (
                    Visit {
                        node: entity,
                        ..visit
                    },
                    entity.child_by_field_name("lhs"),
                    entity.child_by_field_name("rhs"),
                ),
            )
        }
        _ => Ok(()),
    }
}

fn powershell<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "assignment_expression" => ps_assignment(index, context, visit),
        "script_parameter" => ps_parameter(index, context, visit),
        "foreach_statement" => {
            let Some(name) = named_children(visit.node).find(|node| node.kind() == "variable")
            else {
                return Ok(());
            };
            ps_bind(index, context, (visit, name, BindingType::Unknown))
        }
        "invokation_expression" => {
            let receiver = named_children(visit.node).next();
            let name = named_children(visit.node)
                .find(|node| node.kind() == "member_name")
                .and_then(|node| named_children(node).next());
            member(index, context, (visit, receiver, name))
        }
        _ => Ok(()),
    }
}

fn ps_assignment(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = named_children(visit.node)
        .next()
        .map(leaf)
        .filter(|node| node.kind() == "variable")
    else {
        return Ok(());
    };
    let value = visit.node.child_by_field_name("value").map(leaf);
    let constructed = value
        .filter(|node| node.kind() == "invokation_expression")
        .filter(|node| {
            named_children(*node)
                .find(|child| child.kind() == "member_name")
                .is_some_and(|node| node_text(context, node).eq_ignore_ascii_case("new"))
        })
        .and_then(|node| named_children(node).next())
        .and_then(super::type_node);
    let kind = initializer(context, constructed, visit.scope)?;
    ps_bind(index, context, (visit, name, kind))
}

fn ps_parameter(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let name = named_children(visit.node).find(|node| node.kind() == "variable");
    let annotation = named_children(visit.node)
        .find(|node| node.kind() == "attribute_list")
        .map(leaf)
        .and_then(super::type_node);
    let Some(name) = name else {
        return Ok(());
    };
    let kind = explicit(context, annotation, visit.scope)?;
    ps_bind(index, context, (visit, name, kind))
}

fn ps_bind(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, Node<'_>, BindingType),
) -> Result<(), ExtractError> {
    let (visit, name, kind) = input;
    index.bind_name(
        context,
        NamedBind {
            scope: visit.scope,
            name: node_text(context, name).trim_start_matches('$'),
            kind,
            start: if matches!(visit.node.kind(), "script_parameter" | "foreach_statement") {
                0
            } else {
                visit.node.end_byte()
            },
        },
    )
}

fn leaf(mut node: Node<'_>) -> Node<'_> {
    for _ in 0..MAX_SCRIPT_WRAPPERS {
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
