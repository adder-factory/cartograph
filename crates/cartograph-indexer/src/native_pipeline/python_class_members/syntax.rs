//! Decorator expressions and method binding names require exact verified bytes.

use std::collections::HashMap;

use super::super::{
    ExtractedReference, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind,
    ResolutionIndexContext, StageItemFailure, SymbolId, SymbolKind, size_of, usize_to_u64,
};

const MAX_DECORATOR_CONTEXT_BYTES: usize = 256;
pub(super) const UNIQUE_OCCURRENCE: usize = 1;

pub(super) fn bare_decorator(source: &str, reference: &ExtractedReference) -> bool {
    let (Ok(start), Ok(end)) = (
        usize::try_from(reference.span.start_byte()),
        usize::try_from(reference.span.end_byte()),
    ) else {
        return false;
    };
    if source.get(start..end) != Some(reference.name.as_str()) {
        return false;
    }
    prefix_whitespace(source, start) && suffix_whitespace(source, end)
}

fn prefix_whitespace(source: &str, start: usize) -> bool {
    let lower = start.saturating_sub(MAX_DECORATOR_CONTEXT_BYTES);
    let Some(prefix) = source
        .get(lower..start)
        .and_then(|prefix| prefix.strip_suffix('@'))
    else {
        return false;
    };
    let indentation = match prefix.rsplit_once('\n') {
        Some((_, indentation)) => indentation,
        None if lower == 0 => prefix,
        None => return false,
    };
    indentation.trim().is_empty()
}

fn suffix_whitespace(source: &str, end: usize) -> bool {
    let upper = source
        .len()
        .min(end.saturating_add(MAX_DECORATOR_CONTEXT_BYTES));
    let Some(suffix) = source.get(end..upper) else {
        return false;
    };
    let trailing = match suffix.split_once('\n') {
        Some((trailing, _)) => trailing,
        None if upper == source.len() => suffix,
        None => return false,
    };
    trailing.trim().is_empty()
}

pub(super) fn decorated_owners<'file, Cancel>(
    file: &'file NativeFileFacts,
    valid: impl Fn(&ExtractedReference) -> Result<bool, StageItemFailure>,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<&'file SymbolId, bool>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut owners = HashMap::new();
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if reference.kind != ReferenceKind::Decorates {
            continue;
        }
        let Some(owner) = reference.owner.as_ref() else {
            continue;
        };
        let valid = valid(reference)?;
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(&SymbolId, bool)>()))?;
        owners.try_reserve(1).map_err(|_| StageItemFailure)?;
        owners
            .entry(owner)
            .and_modify(|valid: &mut bool| *valid = false)
            .or_insert(valid);
    }
    Ok(owners)
}

pub(super) fn identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

pub(super) fn method_occurrences<'file, Cancel>(
    (file, source): (&'file NativeFileFacts, &str),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<&'file str, usize>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut names: HashMap<&str, usize> = HashMap::new();
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Method || names.contains_key(symbol.name.as_str()) {
            continue;
        }
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(&str, usize)>()))?;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(symbol.name.as_str(), 0);
    }
    for word in source.split(|character| !identifier(character)) {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if let Some(occurrences) = names.get_mut(word) {
            *occurrences = occurrences.saturating_add(1);
        }
    }
    Ok(names)
}
