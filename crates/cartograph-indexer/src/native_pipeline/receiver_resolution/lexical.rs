//! Receiverless inheritance yields to the immediate callable's indexed bindings.

use cartograph_extract::CallScopeKind;

use super::super::generic_resolution::{call_scope, scoped_members};
use super::{ExtractedReference, ReferenceKind, ResolutionIndex, ResolutionRequest};

pub(super) fn shadowed(index: &ResolutionIndex, reference: &ExtractedReference) -> bool {
    call_scope(index, (reference.owner.as_ref(), reference.span))
        == Some(CallScopeKind::CurrentClass)
        && reference
            .owner
            .as_ref()
            .is_some_and(|owner| !scoped_members(index, owner, &reference.name).is_empty())
}

/// Normalized explicit-instance calls cannot bind a local callable of the same name.
pub(in super::super) fn candidate_eligible(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    candidate: &super::super::ResolutionCandidate,
) -> bool {
    !explicit_instance(index, request)
        || candidate.parent_symbol_id.as_ref()
            == request.owner.and_then(|owner| index.parents.get(owner))
}

pub(in super::super) fn explicit_instance(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
) -> bool {
    request.kind == ReferenceKind::Calls
        && call_scope(index, (request.owner, request.span)) == Some(CallScopeKind::CurrentInstance)
}
