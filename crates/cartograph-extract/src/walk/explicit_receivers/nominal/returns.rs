//! Class returns are admitted from annotations in their lexical declaration scope.

use super::{
    ExtractError, ExtractionContext, MemberSite, Node, SourceLanguage, SyntaxIndex, TypeQuery,
    Visit, named_children, node_text, type_node,
};
use crate::ExtractedReceiverLookup;

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    let Some(annotation) = annotation(context.snapshot.language(), visit.node).and_then(type_node)
    else {
        return Ok(());
    };
    super::reserve_slot(context, &mut index.declared_returns)?;
    index.declared_returns.push(MemberSite {
        receiver: annotation,
        member: visit.node,
        scope: visit.scope,
        start: annotation.start_byte(),
        end: annotation.end_byte(),
        literal: true,
    });
    Ok(())
}

fn annotation(language: SourceLanguage, node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "method_declaration" | "function_definition" => node
            .child_by_field_name("return_type")
            .or_else(|| node.child_by_field_name("returns"))
            .or_else(|| node.child_by_field_name("type")),
        "method_definition" | "function_declaration" => {
            node.child_by_field_name("return_type").or_else(|| {
                matches!(language, SourceLanguage::Kotlin | SourceLanguage::Swift)
                    .then(|| named_children(node).find(|child| child.kind() == "user_type"))
                    .flatten()
            })
        }
        _ => None,
    }
}

pub(in super::super) fn lookup(
    index: &SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    site: MemberSite<'_>,
) -> Result<Option<ExtractedReceiverLookup>, ExtractError> {
    let receiver = index.types().explicit_type(
        context,
        TypeQuery {
            scope: site.scope,
            name: node_text(context, site.receiver),
            class_bindings: true,
            position: Some(site.start),
        },
    )?;
    let Some(receiver) = receiver.filter(|name| name != "?") else {
        return Ok(None);
    };
    let lookup = super::prefixed_type(
        context,
        (crate::EXPLICIT_RECEIVER_RESOLUTION_PREFIX, &receiver),
    )?;
    let lookup = super::prefixed_type(context, (&lookup, "#"))?;
    Ok(Some(ExtractedReceiverLookup {
        span: super::super::super::syntax::span_for(site.member)?,
        kind: cartograph_domain::ReferenceKind::Returns,
        lookup,
    }))
}
