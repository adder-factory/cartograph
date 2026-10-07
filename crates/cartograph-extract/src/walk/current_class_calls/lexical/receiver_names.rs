//! Conservative receiver binding tokens share the existing callable scan/cache.

use super::{ExtractError, ExtractionBuilder, HashSet, Node, cache_method, token_name};

const TOKEN_ALLOCATION_ALLOWANCE: u64 = 128;
pub(super) const METHOD_CACHE_ALLOWANCE: u64 = 128;

#[derive(Default)]
pub(super) struct MethodNames {
    pub(super) non_calls: HashSet<String>,
    pub(super) receiver_bindings: HashSet<String>,
}

pub(super) fn retain_token(
    builder: &mut ExtractionBuilder<'_, '_>,
    (names, text, fold): (&mut HashSet<String>, &str, bool),
) -> Result<(), ExtractError> {
    builder.context.budget.reserve_working_bytes(
        TOKEN_ALLOCATION_ALLOWANCE.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX)),
    )?;
    let name = token_name(text, fold)?;
    names
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    names.insert(name);
    Ok(())
}

pub(in super::super::super) fn clean_receiver(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    if !name.is_ascii() {
        return Ok(false);
    }
    cache_method(builder, scope)?;
    Ok(builder
        .current_class_calls
        .methods
        .get(&scope.id())
        .is_some_and(|evidence| !evidence.names.receiver_bindings.contains(name)))
}
