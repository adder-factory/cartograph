//! Arrow access is proven only for a declared raw pointer, never operator->.

use super::{
    BindingType, ExtractError, ExtractionContext, MemberSite, Node, ReceiverTypes, SourceLanguage,
    TypeQuery, named_children, node_text,
};

pub(super) fn declarator_kind(
    context: &ExtractionContext<'_, '_>,
    input: (BindingType, Node<'_>),
) -> BindingType {
    let (mut kind, declarator) = input;
    if context.snapshot.language() == SourceLanguage::Cpp
        && let BindingType::Explicit { raw_pointer, .. } = &mut kind
    {
        *raw_pointer = raw_pointer_declarator(declarator);
    }
    kind
}

fn raw_pointer_declarator(mut node: Node<'_>) -> bool {
    let mut pointer = false;
    for _ in 0..super::MAX_TYPE_WRAPPERS {
        match node.kind() {
            "identifier" | "field_identifier" => return pointer,
            "pointer_declarator" if !pointer => pointer = true,
            "reference_declarator" | "init_declarator" => {}
            _ => return false,
        }
        let Some(child) = node
            .child_by_field_name("declarator")
            .or_else(|| named_children(node).next())
        else {
            return false;
        };
        node = child;
    }
    false
}

pub(super) fn permits_access(
    types: &ReceiverTypes<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (MemberSite<'_>, usize),
) -> Result<bool, ExtractError> {
    let (site, depth) = input;
    if context.snapshot.language() != SourceLanguage::Cpp || !arrow_receiver(site.receiver) {
        return Ok(true);
    }
    let binding = if site.receiver.kind() == "identifier" {
        types.find_binding(
            context,
            TypeQuery {
                name: node_text(context, site.receiver),
                scope: site.scope,
                class_bindings: true,
                position: Some(site.start),
            },
        )?
    } else {
        super::expressions::field_binding(types, context, (site, depth))?
    };
    Ok(binding.is_some_and(|binding| {
        matches!(
            binding.kind,
            BindingType::Explicit {
                raw_pointer: true,
                ..
            }
        )
    }))
}

fn arrow_receiver(receiver: Node<'_>) -> bool {
    receiver.parent().is_some_and(|parent| {
        parent.kind() == "field_expression"
            && parent
                .child_by_field_name("argument")
                .is_some_and(|node| node.id() == receiver.id())
            && parent
                .child_by_field_name("operator")
                .is_some_and(|node| node.kind() == "->")
    })
}
