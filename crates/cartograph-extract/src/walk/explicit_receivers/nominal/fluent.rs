use super::expressions::member_parts;
use super::{
    ExtractError, ExtractionContext, Node, SourceLanguage, SyntaxIndex, Visit, bind_typed,
    bind_unknown, initialized, member, named_children,
};

pub(super) fn supported(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::Kotlin
            | SourceLanguage::Swift
            | SourceLanguage::Scala
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    )
}

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    if matches!(
        context.snapshot.language(),
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    ) {
        super::super::javascript::collect(index, context, visit)?;
    }
    match visit.node.kind() {
        "navigation_expression" | "member_expression" | "field_expression" => {
            let Some((receiver, name)) = member_parts(visit.node) else {
                return Ok(());
            };
            member(index, context, (visit, Some(receiver), Some(name)))
        }
        "call_expression" => call(index, context, visit),
        "parameter" | "class_parameter" | "lambda_parameter" | "parameter_with_optional_type" => {
            parameter(index, context, visit)
        }
        "required_parameter" | "optional_parameter" => bind_typed(
            index,
            context,
            (
                visit,
                visit.node.child_by_field_name("pattern"),
                visit.node.child_by_field_name("type"),
            ),
        ),
        "public_field_definition" | "field_definition" | "variable_declarator" => {
            variable(index, context, visit)
        }
        "property_declaration" => property(index, context, visit),
        "val_definition" | "var_definition" => declared(
            index,
            context,
            (
                visit,
                visit.node.child_by_field_name("pattern"),
                visit.node.child_by_field_name("type"),
            ),
        ),
        "assignment_expression" | "assignment" => {
            super::bindings::assignment(index, context, visit)
        }
        "type_parameter" | "for_statement" => bind_unknown(
            index,
            context,
            (
                visit,
                visit
                    .node
                    .child_by_field_name("name")
                    .or_else(|| visit.node.child_by_field_name("item"))
                    .or_else(|| named_children(visit.node).next()),
            ),
        ),
        "lambda_expression" => bind_unknown(
            index,
            context,
            (visit, visit.node.child_by_field_name("parameters")),
        ),
        _ => Ok(()),
    }
}

fn parameter(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let annotation = visit.node.child_by_field_name("type").or_else(|| {
        named_children(visit.node).find(|node| matches!(node.kind(), "user_type" | "nullable_type"))
    });
    let name = named_children(visit.node)
        .filter(|node| matches!(node.kind(), "simple_identifier" | "identifier"))
        .last();
    bind_typed(
        index,
        context,
        (
            visit,
            visit
                .node
                .child_by_field_name("name")
                .filter(|node| node.kind() == "identifier")
                .or(name),
            annotation,
        ),
    )
}

fn variable(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    declared(
        index,
        context,
        (
            visit,
            visit.node.child_by_field_name("name"),
            visit.node.child_by_field_name("type"),
        ),
    )
}

fn declared(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, Option<Node<'_>>, Option<Node<'_>>),
) -> Result<(), ExtractError> {
    let (visit, name, annotation) = input;
    if annotation.is_some() {
        return bind_typed(index, context, (visit, name, annotation));
    }
    initialized(
        index,
        context,
        (visit, name, visit.node.child_by_field_name("value")),
    )
}

fn property(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let declaration = named_children(visit.node).find(|node| node.kind() == "variable_declaration");
    if let Some(declaration) = declaration {
        let name = named_children(declaration).find(|node| node.kind() == "simple_identifier");
        let annotation = named_children(declaration)
            .find(|node| matches!(node.kind(), "user_type" | "nullable_type"));
        if annotation.is_some() {
            return bind_typed(index, context, (visit, name, annotation));
        }
        let value = named_children(visit.node).find(|node| node.kind() == "call_expression");
        return initialized(index, context, (visit, name, value));
    }
    let name = visit
        .node
        .child_by_field_name("name")
        .and_then(|node| node.child_by_field_name("bound_identifier").or(Some(node)));
    let annotation = named_children(visit.node).find(|node| node.kind() == "type_annotation");
    declared(index, context, (visit, name, annotation))
}

fn call<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let target = visit
        .node
        .child_by_field_name("function")
        .or_else(|| named_children(visit.node).next());
    let Some((receiver, name)) = target.and_then(member_parts) else {
        return Ok(());
    };
    member(index, context, (visit, Some(receiver), Some(name)))
}
