//! Literal, direct class-body includes supply mixin inheritance evidence.

use cartograph_domain::{ReferenceKind, SymbolKind};
use tree_sitter::Node;

use super::super::{ExtractionBuilder, PendingReference, references, syntax::named_children};
use crate::ExtractError;

pub(super) fn capture(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
) -> Result<(), ExtractError> {
    if !direct_include(builder, call) {
        return Ok(());
    }
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Ok(());
    };
    for argument in named_children(arguments) {
        builder.context.ensure_active()?;
        if !super::is_constant_path(argument, 0) {
            continue;
        }
        let Some(name) = super::bounded_name(builder, argument)? else {
            continue;
        };
        references::push_reference(
            builder,
            PendingReference {
                owner: builder.owners.last().cloned(),
                name,
                kind: ReferenceKind::Inherits,
                node: argument,
            },
        )?;
    }
    Ok(())
}

fn direct_include(builder: &ExtractionBuilder<'_, '_>, call: Node<'_>) -> bool {
    !builder.script.ruby.include_blocked
        && call.kind() == "call"
        && call.child_by_field_name("receiver").is_none()
        && call
            .child_by_field_name("method")
            .is_some_and(|method| builder.context.text(method) == "include")
        && call
            .parent()
            .is_some_and(|parent| parent.kind() == "body_statement")
        && builder
            .native_owner_kinds
            .last()
            .is_some_and(|kind| matches!(kind, SymbolKind::Class | SymbolKind::Module))
}
