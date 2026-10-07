//! A goal can explicitly refer to itself; ordinary calls and def-use sites
//! retain the existing self-edge policy.
use super::{
    ReferenceKind, ResolutionIndex, ResolutionRequest, ResolvedTarget, StageItemFailure,
    SymbolKind, resolution_candidates_for_file, select_candidate,
};

pub(super) fn retain_self_edge(
    language: &str,
    reference: ReferenceKind,
    target: SymbolKind,
) -> bool {
    language == "osiris" && reference == ReferenceKind::References && target == SymbolKind::Module
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !retain_self_edge(request.language, request.kind, SymbolKind::Module) {
        return Ok(None);
    }
    let candidates = resolution_candidates_for_file(index, request.name, request.file_id);
    let candidate = select_candidate(
        candidates,
        |candidate| {
            candidate.kind == SymbolKind::Module
                && candidate.qualified_name == request.name
                && request.owner == Some(&candidate.symbol_id)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: 1.0,
        provenance: "native-resource-self-reference",
    }))
}
