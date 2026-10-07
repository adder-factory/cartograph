//! Single-step nominal receiver proof; declarations or unknown scopes abstain.

use crate::{CallScopeKind, ExtractError, ExtractedCallScopeSite};
use tree_sitter::Node;

use super::{ExtractionBuilder, PendingReference, ReferenceKind, SourceLanguage};

const MAX_SCOPE_HOPS: usize = 64;

pub(super) fn capture(
    builder: &mut ExtractionBuilder<'_, '_>,
    pending: &PendingReference<'_>,
) -> Result<(), ExtractError> {
    if pending.kind != ReferenceKind::Calls
        || !super::nominal::implicit_language(builder.context.snapshot.language())
    {
        return Ok(());
    }
    let Some((receiver, _)) = pending.name.split_once('.') else {
        return Ok(());
    };
    if pending.name.matches('.').count() != 1 || matches!(receiver, "this" | "super") {
        return Ok(());
    }
    let Some(owner) = pending.owner.as_ref() else {
        return Ok(());
    };
    if !proven(builder, (pending.node, receiver))? {
        return Ok(());
    }
    super::record_site(
        builder,
        ExtractedCallScopeSite {
            owner: owner.clone(),
            span: super::super::syntax::span_for(pending.node)?,
            kind: CallScopeKind::DirectCallable,
        },
    )
}

fn proven(
    builder: &mut ExtractionBuilder<'_, '_>,
    (node, name): (Node<'_>, &str),
) -> Result<bool, ExtractError> {
    let language = builder.context.snapshot.language();
    let mut scope = node.parent();
    for _ in 0..MAX_SCOPE_HOPS {
        builder.context.ensure_active()?;
        let Some(node) = scope else { break };
        scope = node.parent();
        if super::nominal::opaque_closure(language, node) {
            return Ok(false);
        }
        if language == SourceLanguage::Dart && node.kind() == "function_body" {
            return dart_body(builder, (node, name));
        }
        if super::callable_boundary(node.kind()) || node.kind() == "trigger_declaration" {
            return super::lexical::clean_receiver(builder, node, name);
        }
    }
    Ok(false)
}

fn dart_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    (body, name): (Node<'_>, &str),
) -> Result<bool, ExtractError> {
    let Some(signature) = super::super::dart_family::previous_code_sibling(body) else {
        return Ok(false);
    };
    Ok(
        super::super::dart_family::unshadowed_receiver_parameters(builder, signature, name)?
            && super::lexical::clean_receiver(builder, body, name)?,
    )
}
