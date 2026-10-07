use super::expressions::member_parts;
use super::{
    ExtractError, ExtractionContext, MAX_TYPE_WRAPPERS, Node, SourceLanguage, SyntaxIndex, Visit,
    bind_typed, bind_unknown, initialized, member, named_children,
};

pub(super) fn supported(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::Java
            | SourceLanguage::CSharp
            | SourceLanguage::Apex
            | SourceLanguage::Cpp
            | SourceLanguage::ObjectiveC
            | SourceLanguage::Solidity
    )
}

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "variable_declarator" => variable(index, context, visit),
        "formal_parameter"
        | "parameter"
        | "enhanced_for_statement"
        | "state_variable_declaration"
        | "property_declaration"
        | "variable_declaration" => bind_typed(
            index,
            context,
            (
                visit,
                visit.node.child_by_field_name("name"),
                visit.node.child_by_field_name("type"),
            ),
        ),
        "parameter_declaration" | "declaration" | "field_declaration"
            if matches!(
                context.snapshot.language(),
                SourceLanguage::Cpp | SourceLanguage::ObjectiveC
            ) =>
        {
            declarators(index, context, visit)
        }
        "method_invocation" | "message_expression" => member(
            index,
            context,
            (
                visit,
                visit
                    .node
                    .child_by_field_name("object")
                    .or_else(|| visit.node.child_by_field_name("receiver")),
                visit
                    .node
                    .child_by_field_name("name")
                    .or_else(|| visit.node.child_by_field_name("method")),
            ),
        ),
        "invocation_expression" | "call_expression" => call(index, context, visit),
        "member_expression" | "field_access" | "member_access_expression" | "field_expression" => {
            match member_parts(visit.node) {
                Some((receiver, name)) => {
                    member(index, context, (visit, Some(receiver), Some(name)))
                }
                None => Ok(()),
            }
        }
        "assignment_expression" => super::bindings::assignment(index, context, visit),
        "type_parameter"
        | "type_parameter_declaration"
        | "optional_type_parameter_declaration"
        | "variadic_type_parameter_declaration"
        | "lambda_capture_initializer"
        | "catch_formal_parameter" => bind_unknown(
            index,
            context,
            (
                visit,
                visit
                    .node
                    .child_by_field_name("name")
                    .or_else(|| visit.node.child_by_field_name("left"))
                    .or_else(|| named_children(visit.node).next()),
            ),
        ),
        "inferred_parameters" | "lambda_parameters" | "lambda_capture_specifier" => {
            index.fence(context, visit.scope)
        }
        "lambda_expression" => bind_unknown(
            index,
            context,
            (visit, visit.node.child_by_field_name("parameters")),
        ),
        _ => Ok(()),
    }
}

fn variable(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let annotation = visit
        .node
        .parent()
        .and_then(|parent| parent.child_by_field_name("type"));
    if annotation.is_some_and(|node| node.kind() != "implicit_type") {
        return bind_typed(
            index,
            context,
            (visit, visit.node.child_by_field_name("name"), annotation),
        );
    }
    let value = visit.node.child_by_field_name("value").or_else(|| {
        named_children(visit.node).find(|node| node.kind() == "object_creation_expression")
    });
    initialized(
        index,
        context,
        (visit, visit.node.child_by_field_name("name"), value),
    )
}

fn declarators(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let mut cursor = visit.node.walk();
    for declarator in visit.node.children_by_field_name("declarator", &mut cursor) {
        context.ensure_active()?;
        let Some(name) = declarator_name(declarator) else {
            continue;
        };
        let annotation = visit
            .node
            .child_by_field_name("type")
            .and_then(super::type_node);
        let kind = super::explicit(context, annotation, visit.scope)?;
        let kind = super::cpp::declarator_kind(context, (kind, declarator));
        super::bind_kind(index, context, (visit, name, kind))?;
    }
    Ok(())
}

fn declarator_name(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_TYPE_WRAPPERS {
        match node.kind() {
            "identifier"
            | "field_identifier"
            | "array_declarator"
            | "structured_binding_declarator" => return Some(node),
            "pointer_declarator" | "reference_declarator" | "init_declarator" => {
                node = node
                    .child_by_field_name("declarator")
                    .or_else(|| named_children(node).next())?;
            }
            _ => return None,
        }
    }
    None
}

fn call<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let Some(function) = visit.node.child_by_field_name("function") else {
        return Ok(());
    };
    let function = if function.kind() == "expression" {
        named_children(function).next().unwrap_or(function)
    } else {
        function
    };
    let Some((receiver, name)) = member_parts(function) else {
        return Ok(());
    };
    member(index, context, (visit, Some(receiver), Some(name)))
}
