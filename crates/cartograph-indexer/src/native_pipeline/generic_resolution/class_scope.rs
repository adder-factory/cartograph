use cartograph_extract::{CallScopeKind, ExtractedCallScopeSite};

use super::super::{
    EXACT_LEXICAL_PROVENANCE, ExtractedReference, RESOLUTION_MAP_NODE_ALLOWANCE, SourceSpan,
    usize_to_u64,
};
use super::{
    GenericResolutionIndex, HashMap, ReferenceDispatch, ReferenceKind, ReferenceResolution,
    ResolutionCandidate, ResolutionIndex, ResolutionRequest, ResolveBudget, ResolvedTarget,
    StageItemFailure, callable_kind, compatible_member, implicit_receiver_language, owning_class,
    scoped_names, select_unique_candidate,
};
use std::mem::size_of;

pub(in super::super) fn index_calls<Cancel>(
    index: &mut GenericResolutionIndex,
    (proofs, budget): (&[ExtractedCallScopeSite], &mut ResolveBudget),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for site in proofs {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let owner = &site.owner;
        if !index.call_sites.contains_key(owner) {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<(
                        super::SymbolId,
                        HashMap<(u64, u64), CallScopeKind>,
                    )>()))
                    .saturating_add(usize_to_u64(owner.as_str().len())),
            )?;
            index
                .call_sites
                .try_reserve(1)
                .map_err(|_| StageItemFailure)?;
            index.call_sites.insert(owner.clone(), HashMap::new());
        }
        let sites = index.call_sites.get_mut(owner).ok_or(StageItemFailure)?;
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<((u64, u64), CallScopeKind)>())),
        )?;
        sites.try_reserve(1).map_err(|_| StageItemFailure)?;
        sites.insert((site.span.start_byte(), site.span.end_byte()), site.kind);
    }
    Ok(())
}

pub(in super::super) fn proven_call(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
) -> bool {
    scope(index, (request.owner, request.span)).is_some()
}

pub(in super::super) fn scope(
    index: &ResolutionIndex,
    (owner, span): (Option<&super::SymbolId>, SourceSpan),
) -> Option<CallScopeKind> {
    owner
        .and_then(|owner| index.languages.generic.call_sites.get(owner))
        .and_then(|sites| sites.get(&(span.start_byte(), span.end_byte())))
        .copied()
}

pub(super) fn explicit_instance(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    scope(index, (request.owner, request.span)) == Some(CallScopeKind::CurrentInstance)
}

pub(super) fn reference_proven(index: &ResolutionIndex, reference: &ExtractedReference) -> bool {
    matches!(
        scope(index, (reference.owner.as_ref(), reference.span)),
        Some(CallScopeKind::CurrentClass | CallScopeKind::CurrentInstance)
    )
}

fn lexical_target(candidate: &ResolutionCandidate) -> ReferenceResolution {
    ReferenceResolution::resolved(ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: 1.0,
        provenance: EXACT_LEXICAL_PROVENANCE,
    })
}

/// An unproved or competing binding yields to the existing resolver.
pub(in super::super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::Calls
        || request.dispatch != ReferenceDispatch::Static
        || !implicit_receiver_language(request.language)
        || request.name.contains(['.', ':'])
        || !proven_call(index, request)
    {
        return Ok(None);
    }
    let Some(owner) = request.owner else {
        return Ok(None);
    };
    let Some(class) = owning_class(index, owner) else {
        return Ok(None);
    };
    let locals = scoped_names::candidates(index, request, Some(owner))?;
    if !locals.is_empty() {
        return Ok(
            select_unique_candidate(locals, binding_candidate, cancelled)?
                .filter(|candidate| callable_kind(candidate.kind))
                .map(lexical_target),
        );
    }
    let members = scoped_names::candidates(index, request, Some(class))?;
    let member = select_unique_candidate(members, binding_candidate, cancelled)?;
    Ok(member
        .filter(|candidate| {
            callable_kind(candidate.kind)
                && compatible_member(index, (owner, &candidate.symbol_id), request)
        })
        .map(lexical_target))
}

fn binding_candidate(candidate: &ResolutionCandidate) -> bool {
    !candidate.augmentation && !super::framework_landmark_candidate(candidate)
}
