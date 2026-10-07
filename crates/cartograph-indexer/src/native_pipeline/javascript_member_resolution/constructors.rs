//! Members of a directly constructed, lexical class are syntax-proven.

use super::super::{
    DYNAMIC_DISPATCH_CONFIDENCE, DYNAMIC_DISPATCH_PROVENANCE, MAX_SYMBOL_QUALIFIED_NAME_BYTES,
    ResolvedTarget, nominal_scope_resolution,
};
use super::{
    ReferenceKind, ReferenceResolution, ResolutionIndex, ResolutionRequest, StageItemFailure,
    SymbolKind, Visibility, constructor_member_name, resolution_candidates_for_file,
    unique_candidate,
};

pub(super) fn local<Cancel>(
    index: &ResolutionIndex,
    (request, constructor): (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(
        super::companion(index, request),
        Some(super::Companion::Constructor {
            local_proven: true,
            ..
        })
    ) {
        return Ok(None);
    }
    let lookup = ResolutionRequest {
        name: constructor,
        kind: ReferenceKind::FieldAccess,
        ..*request
    };
    let Some(class) = nominal_scope_resolution::resolve(index, &lookup, cancelled)?
        .and_then(|resolution| resolution.target)
        .filter(|target| target.kind == SymbolKind::Class)
        .and_then(|target| {
            index
                .javascript_members
                .classes
                .get(&target.symbol_id)
                .map(|class| (target.symbol_id, class))
        })
    else {
        return Ok(None);
    };
    let mut buffer = [0_u8; MAX_SYMBOL_QUALIFIED_NAME_BYTES];
    let Some(name) = constructor_member_name((&class.1.qualified_name, request.name), &mut buffer)
    else {
        return Ok(None);
    };
    let selected = unique_candidate(
        resolution_candidates_for_file(index, name, &class.1.file_id),
        |candidate| {
            candidate.parent_symbol_id.as_ref() == Some(&class.0)
                && candidate.kind == SymbolKind::Method
                && !candidate.static_member
                && candidate.visibility == Some(Visibility::Public)
                && !candidate.augmentation
        },
        cancelled,
    )?;
    Ok(selected.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: DYNAMIC_DISPATCH_CONFIDENCE,
            provenance: DYNAMIC_DISPATCH_PROVENANCE,
        })
    }))
}
