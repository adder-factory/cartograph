//! Kotlin callable details: extension receivers and annotated parameters.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    ExtractionBuilder, JVM_TYPE_OWNER_KINDS, PendingSymbol, TypeReferenceCapture, annotations,
    capture_type_references, emit_jvm_symbol, kotlin_direct_name, kotlin_type_node, named_children,
    safe_signature,
};

/// Head type name of a top-level Kotlin extension function's receiver, such as
/// `String` for `fun String.shout()` or `Cfg` for `fun com.demo.Cfg?.x()`.
///
/// The receiver qualifies the function's identity (v1 `getReceiverType`):
/// the head becomes an extra qualifier (`pkg::String::shout`). Lexical
/// ownership is unchanged, so no containment is invented for a same-named type
/// and the result never depends on declaration order. `None` for an ordinary
/// function or a member extension declared inside a type body.
pub(super) fn extension_receiver(
    builder: &mut ExtractionBuilder<'_, '_>,
    function: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if super::current_owner_kind_in(builder, JVM_TYPE_OWNER_KINDS) {
        return Ok(None);
    }
    let Some(user_type) = function
        .child_by_field_name("receiver")
        .and_then(receiver_user_type)
    else {
        return Ok(None);
    };
    // The receiver path is kept as written (`a.Cfg` and `b.Cfg` stay distinct);
    // generic arguments are not part of the name.
    let mut path = String::new();
    for segment in named_children(user_type).filter(|child| child.kind() == "type_identifier") {
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(builder.context.text(segment).trim());
    }
    if path.is_empty() {
        return Ok(None);
    }
    builder.context.copy_text(&path).map(Some)
}

/// The receiver's outer `user_type`, skipping `@Ann` type modifiers and looking
/// through nullable (`Cfg?`) and parenthesized (`(Cfg)`) wrappers.
fn receiver_user_type(receiver: Node<'_>) -> Option<Node<'_>> {
    receiver_parts(receiver).map(|parts| parts.user_type)
}

/// A receiver's outer `user_type` and every `type_modifiers` node passed on the
/// way to it (`fun @Ann C.x()`, `fun (@Ann C)?.x()`).
struct ReceiverParts<'tree> {
    user_type: Node<'tree>,
    modifiers: Vec<Node<'tree>>,
}

/// Unwrap the receiver to its `user_type`, or `None` for any other shape.
fn receiver_parts(receiver: Node<'_>) -> Option<ReceiverParts<'_>> {
    let mut modifiers = Vec::new();
    let mut current = receiver;
    for _ in 0..MAX_RECEIVER_WRAPPERS {
        modifiers.extend(named_children(current).filter(|child| child.kind() == "type_modifiers"));
        current = named_children(current).find(|child| child.kind() != "type_modifiers")?;
        match current.kind() {
            "user_type" => {
                return Some(ReceiverParts {
                    user_type: current,
                    modifiers,
                });
            }
            "nullable_type" | "parenthesized_type" => {}
            _ => return None,
        }
    }
    None
}

/// `TypeOf` references for an extension receiver.
///
/// A written receiver path names one type, so only its terminal segment is a
/// type use (`fun com.demo.Cfg.x()` references `Cfg`, never the package
/// segments `com` and `demo`); generic arguments and receiver type annotations
/// are ordinary type uses. Any other receiver shape (a function type, for
/// example) is captured like every other Kotlin type position.
pub(super) fn capture_receiver_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    receiver: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(ReceiverParts {
        user_type,
        modifiers,
    }) = receiver_parts(receiver)
    else {
        return capture_type_references(
            builder,
            TypeReferenceCapture {
                node: receiver,
                owner,
                kind: ReferenceKind::TypeOf,
                depth: 0,
            },
        );
    };
    let terminal = named_children(user_type)
        .filter(|child| child.kind() == "type_identifier")
        .last();
    let arguments = named_children(user_type).filter(|child| child.kind() == "type_arguments");
    // Type annotations on the receiver (`@Ann C`) stay ordinary type uses.
    for node in modifiers.into_iter().chain(terminal).chain(arguments) {
        capture_type_references(
            builder,
            TypeReferenceCapture {
                node,
                owner,
                kind: ReferenceKind::TypeOf,
                depth: 1,
            },
        )?;
    }
    Ok(())
}

/// Nullable/parenthesized wrappers unwrapped around a receiver type.
const MAX_RECEIVER_WRAPPERS: usize = 8;

/// Emit a `Parameter` symbol for every annotated parameter of a Kotlin callable.
///
/// Kotlin places a parameter's annotations in a sibling `parameter_modifiers`
/// node immediately before the `parameter` (v1 state machine): a modifiers node
/// pairs only with the very next parameter, and any other node resets the
/// pairing. Unannotated parameters are skipped. The caller has already pushed
/// the callable as the current owner and qualifier.
pub(super) fn emit_annotated_parameters(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
) -> Result<(), ExtractError> {
    let mut pending_modifiers = None;
    for child in named_children(parameters) {
        builder.context.ensure_active()?;
        match child.kind() {
            "parameter_modifiers" => pending_modifiers = Some(child),
            "parameter" => {
                if let Some(modifiers) = pending_modifiers.take()
                    && has_annotation(modifiers)
                {
                    emit_annotated_parameter(builder, child, modifiers)?;
                }
            }
            _ => pending_modifiers = None,
        }
    }
    Ok(())
}

fn has_annotation(modifiers: Node<'_>) -> bool {
    named_children(modifiers).any(|child| child.kind() == "annotation")
}

fn emit_annotated_parameter(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameter: Node<'_>,
    modifiers: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = kotlin_direct_name(parameter) else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let parameter_text = builder.context.owned_text(parameter)?;
    let signature = safe_signature(builder, parameter_text)?;
    let id = emit_jvm_symbol(
        builder,
        PendingSymbol {
            kind: SymbolKind::Parameter,
            name,
            span_node: parameter,
            structural_node: parameter,
            doc_anchor: parameter,
            body_node: None,
            declaration_only: false,
            signature,
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        },
    )?;
    annotations::capture_annotations(builder, modifiers, &id)?;
    let Some(type_node) = kotlin_type_node(parameter) else {
        return Ok(());
    };
    capture_type_references(
        builder,
        TypeReferenceCapture {
            node: type_node,
            owner: &id,
            kind: ReferenceKind::TypeOf,
            depth: 0,
        },
    )
}
