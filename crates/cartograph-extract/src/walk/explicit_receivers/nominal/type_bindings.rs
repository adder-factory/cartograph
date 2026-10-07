//! Type declarations outside the admitted class set cannot prove a namesake.

use super::{
    ExtractError, ExtractionContext, ScopeKind, SourceLanguage, SyntaxIndex, Visit, bind_unknown,
};

pub(super) fn collect(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    if reopens_type(context.snapshot.language(), visit.node) {
        return Ok(());
    }
    let Some(parent) = index
        .scopes
        .get(&visit.node.id())
        .filter(|scope| scope.kind == ScopeKind::Class && scope.nominal.is_none())
        .and_then(|scope| scope.parent)
    else {
        return Ok(());
    };
    let name = visit
        .node
        .child_by_field_name("name")
        .or_else(|| super::declaration_name(context.snapshot.language(), visit.node));
    bind_unknown(
        index,
        context,
        (
            Visit {
                scope: parent,
                ..visit
            },
            name,
        ),
    )
}

fn reopens_type(language: SourceLanguage, node: super::Node<'_>) -> bool {
    node.kind() == "class_implementation"
        || language == SourceLanguage::Swift
            && node
                .child_by_field_name("declaration_kind")
                .is_some_and(|kind| kind.kind() == "extension")
}
