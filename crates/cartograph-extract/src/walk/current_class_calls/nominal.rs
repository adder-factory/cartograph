//! Additional class families use the same bounded call-scope evidence.

use cartograph_domain::SourceLanguage;
use tree_sitter::Node;

use super::super::ExtractionBuilder;
use crate::{CallScopeKind, ExtractError};

pub(super) fn explicit_instance(language: SourceLanguage, node: Node<'_>) -> bool {
    language == SourceLanguage::Dart
        && super::super::dart_family::current_call(node) == Some(CallScopeKind::CurrentInstance)
}

pub(super) fn current_kind(language: SourceLanguage, node: Node<'_>) -> CallScopeKind {
    if explicit_instance(language, node) {
        CallScopeKind::CurrentInstance
    } else {
        CallScopeKind::CurrentClass
    }
}

pub(super) fn scope_proof(
    builder: &mut ExtractionBuilder<'_, '_>,
    (node, pending, explicit): (Node<'_>, &super::PendingReference<'_>, bool),
) -> Result<Option<bool>, ExtractError> {
    if let Some(proof) = proven_sibling_body(builder, node, (&pending.name, explicit))? {
        return Ok(Some(proof));
    }
    let language = builder.context.snapshot.language();
    if proven_field_receiver(language, node, explicit) {
        return Ok(Some(true));
    }
    Ok(opaque_closure(language, node).then_some(false))
}

pub(super) fn implicit_language(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::Apex
            | SourceLanguage::Dart
            | SourceLanguage::Groovy
            | SourceLanguage::Scala
    )
}

pub(super) fn receiver_call(language: SourceLanguage, name: &str) -> bool {
    implicit_language(language) && name.starts_with("this.")
}

pub(super) fn method_body(language: SourceLanguage, node: Node<'_>) -> bool {
    match language {
        SourceLanguage::Apex => matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        ),
        SourceLanguage::Groovy | SourceLanguage::Scala => node.kind() == "function_definition",
        SourceLanguage::Ruby => node.kind() == "singleton_method",
        _ => false,
    }
}

pub(super) fn opaque_closure(language: SourceLanguage, node: Node<'_>) -> bool {
    language == SourceLanguage::Groovy
        && node.kind() == "closure"
        && !node.parent().is_some_and(|parent| {
            parent.kind() == "function_definition"
                && parent.child_by_field_name("body") == Some(node)
        })
}

pub(super) fn clean_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    method: Node<'_>,
    pending: &super::PendingReference<'_>,
) -> Result<bool, ExtractError> {
    if builder.context.snapshot.language() == SourceLanguage::Ruby {
        // The Ruby walker already excludes local-variable reads before it
        // emits a bare call. Parenthesized calls always invoke a method.
        return Ok(true);
    }
    super::lexical::clean_call(builder, method, &pending.name)
}

fn proven_field_receiver(language: SourceLanguage, node: Node<'_>, explicit: bool) -> bool {
    explicit
        && matches!(
            language,
            SourceLanguage::JavaScript
                | SourceLanguage::Jsx
                | SourceLanguage::TypeScript
                | SourceLanguage::Tsx
                | SourceLanguage::ArkTs
        )
        && matches!(node.kind(), "field_definition" | "public_field_definition")
        && super::directly_in_type(node)
}

fn proven_sibling_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    body: Node<'_>,
    (name, explicit): (&str, bool),
) -> Result<Option<bool>, ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Dart || body.kind() != "function_body"
    {
        return Ok(None);
    }
    let signature = super::super::dart_family::previous_code_sibling(body);
    let Some(signature) = signature.filter(|node| node.kind() == "method_signature") else {
        return Ok(Some(false));
    };
    if !super::directly_in_type(signature) {
        return Ok(Some(false));
    }
    Ok(Some(
        explicit
            || (super::lexical::clean_call(builder, signature, name)?
                && super::lexical::clean_call(builder, body, name)?),
    ))
}
