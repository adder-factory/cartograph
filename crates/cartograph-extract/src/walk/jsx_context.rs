//! Exclude unproven JSX Context receivers from the new convention fallback.
use super::{ExtractionBuilder, javascript_scopes};
use crate::ExtractError;
use cartograph_domain::{ReferenceKind, SourceLanguage};
use tree_sitter::Node;

/// Exclude only the Context fallback while retaining ordinary reference lookup.
pub const JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX: &str = "framework-react-context-unbound::";

pub(super) fn resolution_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    target: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if target.kind() != "member_expression" {
        return Ok(None);
    }
    let Some(receiver) = target
        .child_by_field_name("object")
        .filter(|receiver| receiver.kind() == "identifier")
    else {
        return Ok(None);
    };
    if !context_member_name(builder.context.text(target)) {
        return Ok(None);
    }
    let binding = javascript_scopes::read_binding(builder, receiver)?;
    if binding.module_bound && binding.resolves_by_name() {
        return Ok(None);
    }
    let name = builder.context.text(target);
    let capacity = JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX
        .len()
        .saturating_add(name.len());
    builder.context.budget.ensure_string_length(capacity)?;
    excluded_lookup(name).map(Some)
}

pub(crate) fn non_jsx_resolution_name(
    (language, kind, name): (SourceLanguage, ReferenceKind, &str),
) -> Result<Option<String>, ExtractError> {
    if kind != ReferenceKind::References
        || !matches!(language, SourceLanguage::Jsx | SourceLanguage::Tsx)
        || !context_member_name(name)
    {
        return Ok(None);
    }
    excluded_lookup(name).map(Some)
}

fn context_member_name(name: &str) -> bool {
    name.strip_suffix(".Provider")
        .or_else(|| name.strip_suffix(".Consumer"))
        .is_some_and(|receiver| receiver.ends_with("Context") && !receiver.contains(['.', ':']))
}

fn excluded_lookup(name: &str) -> Result<String, ExtractError> {
    let capacity = JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX
        .len()
        .saturating_add(name.len());
    let mut resolution = String::new();
    resolution
        .try_reserve(capacity)
        .map_err(|_| ExtractError::OutputLimit)?;
    resolution.push_str(JSX_CONTEXT_UNBOUND_RESOLUTION_PREFIX);
    resolution.push_str(name);
    Ok(resolution)
}
