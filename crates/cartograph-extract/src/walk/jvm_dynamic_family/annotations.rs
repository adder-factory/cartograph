//! `@Annotation` decorator references for Kotlin, Scala, and Groovy declarations.
//!
//! Kotlin nests annotations in a `modifiers` (or `parameter_modifiers`) wrapper
//! and names them through a `user_type`, optionally inside a
//! `constructor_invocation` when arguments are present. Scala exposes the name
//! through the annotation's `name` field, and Groovy through a direct
//! `identifier`. Every shape yields one `Decorates` reference named by the
//! dotted annotation type, matching the Java managed family.

use cartograph_domain::{ReferenceKind, SymbolId};
use tree_sitter::Node;

use crate::ExtractError;

use super::{ExtractionBuilder, PendingReference, named_children, push_named_reference};

/// Wrapper kinds whose direct children may be annotations.
const ANNOTATION_WRAPPERS: &[&str] = &["modifiers", "parameter_modifiers"];

/// Record every annotation attached to `declaration` as a `Decorates` reference
/// owned by `owner`.
///
/// Only direct children and one wrapper level are inspected, so annotations of
/// nested declarations are never attributed to their container.
pub(super) fn capture_annotations(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for child in named_children(declaration) {
        builder.context.ensure_active()?;
        if child.kind() == "annotation" {
            capture_annotation(builder, child, owner)?;
        } else if ANNOTATION_WRAPPERS.contains(&child.kind()) {
            for annotation in named_children(child).filter(|node| node.kind() == "annotation") {
                builder.context.ensure_active()?;
                capture_annotation(builder, annotation, owner)?;
            }
        }
    }
    Ok(())
}

fn capture_annotation(
    builder: &mut ExtractionBuilder<'_, '_>,
    annotation: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(name_node) = annotation_name(annotation) else {
        return Ok(());
    };
    let Some(name) = super::safe_type_text(builder, name_node)? else {
        return Ok(());
    };
    push_named_reference(
        builder,
        PendingReference {
            owner: Some(owner.clone()),
            name,
            kind: ReferenceKind::Decorates,
            node: name_node,
        },
    )
}

fn annotation_name(annotation: Node<'_>) -> Option<Node<'_>> {
    if let Some(name) = annotation.child_by_field_name("name") {
        return Some(name);
    }
    named_children(annotation).find_map(|child| match child.kind() {
        "user_type"
        | "identifier"
        | "type_identifier"
        | "stable_type_identifier"
        | "qualified_name" => Some(child),
        "constructor_invocation" => {
            named_children(child).find(|candidate| candidate.kind() == "user_type")
        }
        _ => None,
    })
}
