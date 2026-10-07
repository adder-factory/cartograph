//! Unsupported binders fence their exact lexical region rather than reuse an
//! enclosing value or a project class with the same identifier.

use super::{
    ExtractError, ExtractionContext, SourceLanguage, SyntaxIndex, Visit, bind_unknown,
    named_children,
};

pub(super) fn collect(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "alias_declaration"
        | "namespace_alias_definition"
        | "type_definition"
        | "typealias_declaration"
        | "type_alias"
        | "type_alias_declaration"
            if alias_language(context.snapshot.language()) =>
        {
            alias(index, context, visit)
        }
        "using_directive" if context.snapshot.language() == SourceLanguage::CSharp => {
            using_alias(index, context, visit)
        }
        "genericTpl" if context.snapshot.language() == SourceLanguage::Pascal => {
            fence_file(index, context, visit)
        }
        "type_parameters"
            if matches!(
                context.snapshot.language(),
                SourceLanguage::Scala | SourceLanguage::Dart
            ) =>
        {
            type_parameters(index, context, visit)
        }
        "covariant_type_parameter" | "contravariant_type_parameter" => alias(index, context, visit),
        "type_parameter" if context.snapshot.language() == SourceLanguage::Dart => {
            alias(index, context, visit)
        }
        "try_statement" if context.snapshot.language() == SourceLanguage::Dart => {
            index.fence(context, visit.scope)
        }
        "catch_clause" | "catch_block" | "catch_declaration" | "catch_parameters"
        | "for_expression" | "enumerator" | "for_range_loop" | "foreach_statement"
        | "lambda_literal" => index.fence(context, visit.scope),
        _ => Ok(()),
    }
}

// Pascal generic implementations live outside their class AST. Until their
// binders cross that boundary, withhold added file-level type evidence.
fn fence_file(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let mut scope = visit.scope;
    for _ in 0..=super::super::super::MAX_AST_DEPTH {
        context.ensure_active()?;
        let parent = index
            .scopes
            .get(&scope)
            .ok_or(ExtractError::InvalidSpan)?
            .parent;
        let Some(parent) = parent else {
            return index.fence(context, scope);
        };
        scope = parent;
    }
    Err(ExtractError::InvalidSpan)
}

fn using_alias(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = visit.node.child_by_field_name("name") else {
        return Ok(());
    };
    bind_unknown(index, context, (visit, Some(name)))
}

fn alias_language(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::Cpp
            | SourceLanguage::ObjectiveC
            | SourceLanguage::Swift
            | SourceLanguage::Scala
            | SourceLanguage::Kotlin
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    )
}

fn type_parameters(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    for name in named_children(visit.node) {
        context.ensure_active()?;
        if matches!(name.kind(), "identifier" | "type_identifier") {
            bind_unknown(index, context, (visit, Some(name)))?;
        }
    }
    Ok(())
}

fn alias(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let name = visit
        .node
        .child_by_field_name("name")
        .or_else(|| visit.node.child_by_field_name("declarator"))
        .or_else(|| {
            named_children(visit.node).find(|node| {
                matches!(
                    node.kind(),
                    "identifier" | "type_identifier" | "simple_identifier"
                )
            })
        });
    match name {
        Some(name) => bind_unknown(index, context, (visit, Some(name))),
        None => index.fence(context, visit.scope),
    }
}
