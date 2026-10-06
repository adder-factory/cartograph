//! Prefer a unique definition of an explicitly declared C-family callable.
use super::{
    IncludeImplementationQuery, ReferenceKind, ReferenceResolution, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, c_include_family_name,
    resolution_candidates_for_file, select_candidate, unique_include_implementation,
};

pub(super) fn prefer_definition<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, ResolvedTarget),
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, target) = query;
    if !c_include_family_name(request.language) || request.kind != ReferenceKind::Calls {
        return Ok(ReferenceResolution::resolved(target));
    }
    let candidates = resolution_candidates_for_file(index, request.name, request.file_id);
    let declaration = select_candidate(
        candidates,
        |candidate| {
            candidate.symbol_id == target.symbol_id
                && candidate.implementation.declaration_only
                && candidate.export.exported
        },
        cancelled,
    )?;
    let Some(declaration) = declaration else {
        return Ok(ReferenceResolution::resolved(target));
    };
    let definition = unique_include_implementation(
        IncludeImplementationQuery {
            index,
            request,
            declaration,
        },
        cancelled,
    )?;
    Ok(ReferenceResolution::resolved(definition.map_or(
        target,
        |candidate| ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 0.95,
            provenance: "native-c-declaration-definition",
        },
    )))
}
