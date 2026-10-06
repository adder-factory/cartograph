//! Nearest binding lookup for nominal types and member receivers.

use cartograph_domain::SymbolId;

use super::{
    EXACT_LEXICAL_CONFIDENCE, EXACT_LEXICAL_PROVENANCE, EXACT_SAME_FILE_CONFIDENCE,
    EXACT_SAME_FILE_PROVENANCE, ReferenceResolution, ResolutionCandidate, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, UNRESOLVED_PROVENANCE,
    is_lexical_candidate, jvm_resolution, resolution_candidates_for_file, resolve_lexical,
};

const MAX_SCOPE_HOPS: usize = 64;

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.name.contains(['.', ':']) {
        if jvm_resolution::language(request.language) && request.name.contains('.') {
            return Ok(None);
        }
        return resolve_lexical(index, request, cancelled)
            .map(|target| target.map(ReferenceResolution::resolved));
    }
    let candidates = resolution_candidates_for_file(index, request.name, request.file_id);
    let mut scope = request.owner;
    for _ in 0..MAX_SCOPE_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        match scope_choice(
            &ScopeQuery {
                index,
                candidates,
                request,
                scope,
            },
            cancelled,
        )? {
            ScopeChoice::Unique(candidate) => return Ok(Some(resolution(candidate))),
            ScopeChoice::Ambiguous => {
                return Ok(Some(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)));
            }
            ScopeChoice::Absent => {}
        }
        let Some(id) = scope else {
            return Ok(None);
        };
        scope = index.parents.get(id);
    }
    Ok(Some(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)))
}

struct ScopeQuery<'index, 'request> {
    index: &'index ResolutionIndex,
    candidates: &'index [ResolutionCandidate],
    request: &'index ResolutionRequest<'request>,
    scope: Option<&'index SymbolId>,
}

enum ScopeChoice<'index> {
    Absent,
    Ambiguous,
    Unique(&'index ResolutionCandidate),
}

fn scope_choice<'index, Cancel>(
    query: &ScopeQuery<'index, '_>,
    cancelled: &mut Cancel,
) -> Result<ScopeChoice<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut selected = None;
    for candidate in query.candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if query.request.owner == Some(&candidate.symbol_id)
            || candidate.parent_symbol_id.as_ref() != query.scope
            || !is_lexical_candidate(query.request.kind, query.request.name, candidate)
            || !jvm_resolution::value_binding_candidate(query.request, candidate)
            || !jvm_resolution::lexical_type_visible(query.index, (query.request, candidate))
        {
            continue;
        }
        if selected.is_some() {
            return Ok(ScopeChoice::Ambiguous);
        }
        selected = Some(candidate);
    }
    Ok(selected.map_or(ScopeChoice::Absent, ScopeChoice::Unique))
}

fn resolution(candidate: &ResolutionCandidate) -> ReferenceResolution {
    let (confidence, provenance) = if candidate.parent_symbol_id.is_some() {
        (EXACT_LEXICAL_CONFIDENCE, EXACT_LEXICAL_PROVENANCE)
    } else {
        (EXACT_SAME_FILE_CONFIDENCE, EXACT_SAME_FILE_PROVENANCE)
    };
    ReferenceResolution::resolved(ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence,
        provenance,
    })
}
