//! Syntax evidence for calls made directly in a class method's receiver scope.

use cartograph_domain::{ReferenceKind, SourceLanguage};
use tree_sitter::Node;

use crate::{CallScopeKind, ExtractError, ExtractedCallScopeSite};

use super::{ExtractionBuilder, PendingReference};

mod lexical;

pub(super) use lexical::MethodProofIndex;

fn implicit_language(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::Java
            | SourceLanguage::Kotlin
            | SourceLanguage::CSharp
            | SourceLanguage::Cpp
            | SourceLanguage::Cuda
            | SourceLanguage::Swift
            | SourceLanguage::Ruby
    )
}

fn receiver_call(language: SourceLanguage, name: &str) -> bool {
    match language {
        SourceLanguage::Swift => name.starts_with("self."),
        SourceLanguage::JavaScript
        | SourceLanguage::Jsx
        | SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::ArkTs
        | SourceLanguage::Java
        | SourceLanguage::Kotlin
        | SourceLanguage::CSharp
        | SourceLanguage::Cpp
        | SourceLanguage::Cuda => name.starts_with("this."),
        _ => false,
    }
}

fn callable_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "function_declaration"
            | "function_definition"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "lambda_expression"
            | "lambda_literal"
            | "anonymous_function"
            | "anonymous_method_expression"
            | "local_function_statement"
            | "method"
            | "singleton_method"
    )
}

fn method_body(language: SourceLanguage, node: Node<'_>) -> bool {
    let callable = match language {
        SourceLanguage::JavaScript
        | SourceLanguage::Jsx
        | SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::ArkTs => node.kind() == "method_definition",
        SourceLanguage::Java | SourceLanguage::CSharp => {
            matches!(
                node.kind(),
                "method_declaration" | "constructor_declaration"
            )
        }
        SourceLanguage::Kotlin | SourceLanguage::Swift => node.kind() == "function_declaration",
        SourceLanguage::Cpp | SourceLanguage::Cuda => node.kind() == "function_definition",
        SourceLanguage::Ruby => node.kind() == "method",
        _ => false,
    };
    callable && node.child_by_field_name("receiver").is_none()
}

fn directly_in_type(method: Node<'_>) -> bool {
    let Some(body) = method.parent() else {
        return false;
    };
    if !matches!(
        body.kind(),
        "class_body"
            | "struct_body"
            | "declaration_list"
            | "field_declaration_list"
            | "body_statement"
    ) {
        return false;
    }
    body.parent().is_some_and(|container| {
        matches!(
            container.kind(),
            "class_declaration"
                | "abstract_class_declaration"
                | "struct_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "class_specifier"
                | "struct_specifier"
                | "class"
        )
    })
}

fn proven_method(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
    explicit: bool,
) -> Result<bool, ExtractError> {
    let language = builder.context.snapshot.language();
    let mut ancestor = pending.node.parent();
    for _ in 0..crate::MAXIMUM_AST_DEPTH {
        builder.context.ensure_active()?;
        let Some(node) = ancestor else {
            return Ok(false);
        };
        if explicit && lexical_receiver_closure(language, node) {
            ancestor = node.parent();
            continue;
        }
        if callable_boundary(node.kind())
            || language == SourceLanguage::Ruby && matches!(node.kind(), "block" | "do_block")
        {
            return Ok(method_body(language, node)
                && directly_in_type(node)
                && !lexical::explicit_this_parameter(builder, node)?
                && (explicit || lexical::clean_call(builder, node, &pending.name)?));
        }
        ancestor = node.parent();
    }
    Ok(false)
}

fn lexical_receiver_closure(language: SourceLanguage, node: Node<'_>) -> bool {
    node.kind() == "arrow_function"
        && matches!(
            language,
            SourceLanguage::JavaScript
                | SourceLanguage::Jsx
                | SourceLanguage::TypeScript
                | SourceLanguage::Tsx
                | SourceLanguage::ArkTs
        )
}

fn direct_callable(node: Node<'_>, builder: &ExtractionBuilder<'_, '_>) -> bool {
    if builder.context.snapshot.language() == SourceLanguage::VbNet {
        return matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        );
    }
    matches!(
        node.kind(),
        "function_declaration" | "generator_function_declaration"
    ) && node
        .child_by_field_name("name")
        .is_some_and(|declared| !builder.context.text(declared).is_empty())
}

fn vbnet_source_call(builder: &ExtractionBuilder<'_, '_>, pending: &PendingReference<'_>) -> bool {
    if pending
        .node
        .parent()
        .is_some_and(|parent| parent.kind() == "member_access")
    {
        return false;
    }
    let text = builder
        .context
        .text(pending.node)
        .split('(')
        .next()
        .unwrap_or_default()
        .trim();
    text.eq_ignore_ascii_case(&pending.name)
        || text.split_once('.').is_some_and(|(receiver, member)| {
            receiver.eq_ignore_ascii_case("Me") && member.eq_ignore_ascii_case(&pending.name)
        })
}

fn proven_suffix_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<bool, ExtractError> {
    let mut callable = None;
    let mut ancestor = pending.node.parent();
    for _ in 0..crate::MAXIMUM_AST_DEPTH {
        builder.context.ensure_active()?;
        let Some(node) = ancestor else {
            return Ok(callable.is_some());
        };
        if matches!(node.kind(), "template_declaration" | "lambda_expression") {
            return Ok(false);
        }
        if callable.is_none() && node.kind() == "function_definition" {
            callable = Some(node);
        }
        ancestor = node.parent();
    }
    Ok(false)
}

fn proven_direct_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<bool, ExtractError> {
    let language = builder.context.snapshot.language();
    if matches!(language, SourceLanguage::Cpp | SourceLanguage::Cuda) && pending.name.contains("::")
    {
        return proven_suffix_scope(builder, pending);
    }
    if !matches!(
        language,
        SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::ArkTs
            | SourceLanguage::VbNet
    ) || pending.name.contains(['.', ':'])
    {
        return Ok(false);
    }
    if language == SourceLanguage::VbNet && !vbnet_source_call(builder, pending) {
        return Ok(false);
    }
    let mut ancestor = pending.node.parent();
    for _ in 0..crate::MAXIMUM_AST_DEPTH {
        builder.context.ensure_active()?;
        let Some(node) = ancestor else {
            return Ok(false);
        };
        if callable_boundary(node.kind()) {
            return Ok(direct_callable(node, builder)
                && lexical::clean_call(builder, node, &pending.name)?);
        }
        ancestor = node.parent();
    }
    Ok(false)
}

/// A dynamic with environment can shadow any unqualified name, including
/// a recursive owner or an otherwise unique nearby free function.
fn beneath_with(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<bool, ExtractError> {
    let mut ancestor = pending.node.parent();
    for _ in 0..crate::MAXIMUM_AST_DEPTH {
        builder.context.ensure_active()?;
        let Some(node) = ancestor else {
            return Ok(false);
        };
        if node.kind() == "with_statement" {
            return Ok(true);
        }
        ancestor = node.parent();
    }
    Ok(true)
}

fn scope_kind(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<Option<CallScopeKind>, ExtractError> {
    if pending.kind != ReferenceKind::Calls
        || !source_callee_matches(builder, pending)
        || beneath_with(builder, pending)?
    {
        return Ok(None);
    }
    let language = builder.context.snapshot.language();
    if language == SourceLanguage::Ruby
        && pending.node.parent().is_some_and(|call| {
            call.kind() == "call"
                && call
                    .child_by_field_name("receiver")
                    .is_some_and(|receiver| receiver.kind() != "self")
        })
    {
        return Ok(None);
    }
    let explicit = receiver_call(language, &pending.name);
    let implicit = implicit_language(language) && implicit_source_call(language, pending);
    let kind = if proven_direct_call(builder, pending)? {
        CallScopeKind::DirectCallable
    } else if (explicit || implicit) && proven_method(builder, pending, explicit)? {
        CallScopeKind::CurrentClass
    } else {
        return Ok(None);
    };
    Ok(Some(kind))
}

fn source_callee_matches(
    builder: &ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> bool {
    if !matches!(
        builder.context.snapshot.language(),
        SourceLanguage::Swift | SourceLanguage::Kotlin
    ) {
        return true;
    }
    if pending
        .node
        .end_byte()
        .saturating_sub(pending.node.start_byte())
        > super::MAX_SAFE_SIGNATURE_BYTES
    {
        return false;
    }
    // These families can remove a nested call or indexing from a callee's
    // durable name. Only spelling-preserving normalization proves scope.
    builder
        .context
        .text(pending.node)
        .chars()
        .filter(|character| *character != '`' && !character.is_whitespace())
        .eq(pending.name.chars())
}

fn implicit_source_call(language: SourceLanguage, pending: &PendingReference<'_>) -> bool {
    !pending.name.contains(['.', ':'])
        // Java preserves a bare member name when a computed/literal receiver
        // cannot be retained; that normalization cannot prove current-class scope.
        && (language != SourceLanguage::Java || pending.node.child_by_field_name("object").is_none())
}

pub(super) fn capture(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<(), ExtractError> {
    let Some(owner) = pending.owner.as_ref() else {
        return Ok(());
    };
    let Some(kind) = scope_kind(builder, pending)? else {
        return Ok(());
    };
    let site = ExtractedCallScopeSite {
        owner: owner.clone(),
        span: super::syntax::span_for(pending.node)?,
        kind,
    };
    builder.context.budget.reserve_fact(
        crate::budget::call_scope_site_budget_bytes(&site),
        [site.owner.as_str()],
    )?;
    builder
        .facts
        .call_scope_sites
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.facts.call_scope_sites.push(site);
    Ok(())
}
