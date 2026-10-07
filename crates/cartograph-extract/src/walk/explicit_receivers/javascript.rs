//! Direct `this.field` retains its class receiver even when a parameter shares
//! the field's name. Ordinary nested functions establish an unknown `this`.

use super::{
    Bind, BindingType, ExtractError, ExtractionBuilder, ExtractionContext, MemberSite, Node,
    ScopeKind, SyntaxIndex, Visit,
};

pub(super) fn guard_constructor(
    index: &mut SyntaxIndex<'_>,
    builder: &mut ExtractionBuilder<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    if visit.node.kind() != "new_expression"
        || !super::super::module_system::is_javascript_family(builder.context.snapshot.language())
    {
        return Ok(());
    }
    let Some(name) = visit
        .node
        .child_by_field_name("constructor")
        .filter(|node| node.kind() == "identifier")
    else {
        return Ok(());
    };
    if super::super::javascript_scopes::constructor_binding_proven(builder, name)? {
        return Ok(());
    }
    index.bind(
        &mut builder.context,
        Bind {
            scope: visit.scope,
            name,
            kind: BindingType::Unknown,
            start: 0,
        },
    )
}

pub(super) fn scope_kind(node: Node<'_>) -> Option<ScopeKind> {
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" | "class" => Some(ScopeKind::Class),
        "method_definition"
            if node
                .parent()
                .is_some_and(|parent| parent.kind() == "class_body")
                && !is_static(node) =>
        {
            Some(ScopeKind::Callable)
        }
        "public_field_definition" | "field_definition" if is_static(node) => {
            Some(ScopeKind::Barrier)
        }
        "method_definition"
        | "class_static_block"
        | "function_declaration"
        | "function_expression"
        | "generator_function_declaration"
        | "generator_function" => Some(ScopeKind::Barrier),
        _ => None,
    }
}

fn is_static(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == "static")
}

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    if matches!(
        visit.node.kind(),
        "required_parameter" | "optional_parameter"
    ) && visit
        .node
        .child_by_field_name("pattern")
        .is_some_and(|node| node.kind() == "this")
    {
        return index.fence(context, visit.scope);
    }
    if visit.node.kind() != "member_expression" {
        return Ok(());
    }
    let (Some(receiver), Some(member)) = (
        visit.node.child_by_field_name("object"),
        visit.node.child_by_field_name("property"),
    ) else {
        return Ok(());
    };
    if receiver.kind() != "this" {
        return Ok(());
    }
    index.member(
        context,
        MemberSite {
            receiver,
            member,
            scope: visit.scope,
            start: member.start_byte(),
            end: member.end_byte(),
            literal: false,
        },
    )
}
