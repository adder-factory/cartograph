//! A unique explicit instance constructor needs a proven, nonzero call arity.

use super::{
    ExtractedReference, FileId, HashMap, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceKind, ReferenceResolution, ResolutionIndex, ResolutionIndexContext, ResolutionRequest,
    StageItemFailure, SymbolKind, qualtype_resolution::Selection, resolution_candidates_for_file,
    size_of, usize_to_u64,
};

pub(super) const PROVENANCE: &str = "native-csharp-explicit-constructor";
const ARGUMENT_BYTES: usize = 4096;

#[derive(Default)]
pub(super) struct Calls {
    by_file: HashMap<FileId, HashMap<u64, u16>>,
}

pub(super) fn index_syntax<Cancel>(
    index: &mut ResolutionIndex,
    syntax: (&NativeFileFacts, &str),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, source) = syntax;
    if file.file.language != "csharp" {
        return Ok(());
    }
    let mut counts = HashMap::new();
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if reference.kind != ReferenceKind::Instantiates {
            continue;
        }
        let Some(count) = argument_count(source, reference).filter(|count| *count > 0) else {
            continue;
        };
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(u64, u16)>()))?;
        counts.try_reserve(1).map_err(|_| StageItemFailure)?;
        counts.insert(reference.span.start_byte(), count);
    }
    context.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(FileId, HashMap<u64, u16>)>()))
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
    )?;
    index
        .qualtype
        .constructors
        .by_file
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    index
        .qualtype
        .constructors
        .by_file
        .insert(file.file.file_id.clone(), counts);
    Ok(())
}

fn argument_count(source: &str, reference: &ExtractedReference) -> Option<u16> {
    let start = usize::try_from(reference.span.start_byte()).ok()?;
    let end = usize::try_from(reference.span.end_byte()).ok()?;
    if source.get(start..end) != Some(reference.name.as_str()) {
        return None;
    }
    let tail = source.get(end..source.len().min(end.saturating_add(ARGUMENT_BYTES)))?;
    let arguments = tail.trim_start().strip_prefix('(')?.split_once(')')?.0;
    // Nested expressions, quoted literals, named arguments and comments need AST evidence.
    if !arguments.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || byte.is_ascii_whitespace()
            || matches!(byte, b'_' | b'.' | b'+' | b'-' | b',')
    }) {
        return None;
    }
    if arguments.trim().is_empty() {
        return Some(0);
    }
    let mut count = 0_u16;
    for argument in arguments.split(',') {
        if argument.trim().is_empty() {
            return None;
        }
        count = count.checked_add(1)?;
    }
    Some(count)
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "csharp" || request.kind != ReferenceKind::Instantiates {
        return Ok(None);
    }
    let Some(arguments) = index
        .qualtype
        .constructors
        .by_file
        .get(request.file_id)
        .and_then(|counts| counts.get(&request.span.start_byte()))
    else {
        return Ok(None);
    };
    let Some(parent_id) = request.owner.and_then(|owner| index.parents.get(owner)) else {
        return Ok(None);
    };
    let Some(parent) = index.qualtype.owners.get(parent_id) else {
        return Ok(None);
    };
    if parent.kind != SymbolKind::Struct || parent.name.rsplit("::").next() != Some(request.name) {
        return Ok(None);
    }
    let selected = constructors(index, (request, parent_id), cancelled)?;
    let arity_matches = selected
        .candidate
        .and_then(|candidate| index.qualtype.owners.get(&candidate.symbol_id))
        .is_some_and(|owner| owner.parameter_count == *arguments);
    if selected.ambiguous
        || !arity_matches
        || super::qualtype_generics::blocked(index, request, cancelled)?
    {
        return Ok(None);
    }
    Ok(selected.resolution(PROVENANCE, 1.0))
}

fn constructors<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (&ResolutionRequest<'_>, &super::SymbolId),
    cancelled: &mut Cancel,
) -> Result<Selection<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, parent_id) = query;
    let mut selected = Selection::default();
    for candidate in resolution_candidates_for_file(index, request.name, request.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind == SymbolKind::Method
            && candidate.parent_symbol_id.as_ref() == Some(parent_id)
            && candidate.signature.starts_with('(')
            && index
                .qualtype
                .owners
                .get(&candidate.symbol_id)
                .is_some_and(|owner| !owner.static_member)
        {
            selected.retain(candidate);
        }
    }
    Ok(selected)
}
