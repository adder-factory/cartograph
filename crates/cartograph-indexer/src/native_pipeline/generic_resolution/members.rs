//! Positive member refinements over the existing scope and nominal indexes.
//! A missing or ambiguous member leaves the base resolver result authoritative.

use cartograph_domain::Visibility;

use super::super::{nominal_scope_resolution, resolve_reference};
use super::{
    ReferenceDispatch, ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolId, callable_kind,
    compatible_member, framework_landmark_candidate, owning_class, scoped_names,
    select_unique_candidate,
};

const STATIC_CONFIDENCE: f32 = 0.95;
const STATIC_PROVENANCE: &str = "native-qualified-member";
const MAX_OWNER_HOPS: usize = 64;

pub(super) fn compatible(
    index: &ResolutionIndex,
    (owner, target): (&SymbolId, &SymbolId),
    request: &ResolutionRequest<'_>,
) -> bool {
    let Some(owner) = index.languages.generic.kinds.get(owner) else {
        return false;
    };
    let Some(target) = index.languages.generic.kinds.get(target) else {
        return false;
    };
    if request.language == "dart" {
        if target.constructor {
            return false;
        }
        if super::class_scope::explicit_instance(index, request) {
            return !owner.static_member && !target.static_member;
        }
    }
    if request.language == "ruby" {
        return owner.static_member == target.static_member;
    }
    !owner.static_member || target.static_member
}

pub(in super::super) fn refine<Cancel>(
    index: &ResolutionIndex,
    (request, base): (&ResolutionRequest<'_>, ReferenceResolution),
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::Calls || request.dispatch != ReferenceDispatch::Static {
        return Ok(base);
    }
    let target = if request.name.contains('.') {
        static_member(index, request, cancelled)?
    } else {
        current_member(index, request, cancelled)?
    };
    Ok(match target {
        Some(target)
            if base
                .target
                .as_ref()
                .is_none_or(|base| base.symbol_id != target.symbol_id) =>
        {
            ReferenceResolution::resolved(target)
        }
        _ => base,
    })
}

fn current_member<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(
        request.language,
        "apex" | "dart" | "groovy" | "scala" | "ruby"
    ) || request.name.contains(':')
        || !super::class_scope::proven_call(index, request)
    {
        return Ok(None);
    }
    let Some(owner) = request.owner else {
        return Ok(None);
    };
    let Some(class) = owning_class(index, owner) else {
        return Ok(None);
    };
    if !scoped_names::candidates(index, request, Some(owner))?.is_empty() {
        return Ok(None);
    }
    let candidate = select_unique_candidate(
        scoped_names::members(index, class, request.name),
        eligible_member,
        cancelled,
    )?;
    Ok(candidate
        .filter(|candidate| compatible_member(index, (owner, &candidate.symbol_id), request))
        .map(|candidate| member_target(candidate, true)))
}

fn static_member<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(
        request.language,
        "apex" | "dart" | "groovy" | "scala" | "ruby"
    ) {
        return Ok(None);
    }
    if request.language != "ruby" && !super::class_scope::proven_call(index, request) {
        return Ok(None);
    }
    let Some((receiver, member)) = request.name.rsplit_once('.') else {
        return Ok(None);
    };
    if matches!(receiver, "this" | "self" | "super")
        || receiver.contains('.')
        || member.contains(':')
    {
        return Ok(None);
    }
    if !super::super::receiver_resolution::unshadowed_nominal_receiver(
        index,
        (
            request.owner.and_then(|owner| owning_class(index, owner)),
            receiver,
        ),
        cancelled,
    )? {
        return Ok(None);
    }
    let query = ResolutionRequest {
        name: receiver,
        kind: ReferenceKind::FieldAccess,
        ..*request
    };
    let nominal = match nominal_scope_resolution::resolve(index, &query, cancelled)? {
        Some(resolution) => resolution,
        None => resolve_reference(
            index,
            &ResolutionRequest {
                kind: ReferenceKind::TypeOf,
                ..query
            },
            cancelled,
        )?,
    };
    let Some(class) = nominal
        .target
        .filter(|target| super::type_kind(target.kind))
    else {
        return Ok(None);
    };
    let inside = inside_class(index, (request.owner, &class.symbol_id), cancelled)?;
    let candidate = select_unique_candidate(
        scoped_names::members(index, &class.symbol_id, member),
        |candidate| {
            eligible_member(candidate)
                && candidate.static_member
                && visible((request, candidate), inside)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| member_target(candidate, false)))
}

fn eligible_member(candidate: &ResolutionCandidate) -> bool {
    callable_kind(candidate.kind)
        && !candidate.augmentation
        && !framework_landmark_candidate(candidate)
}

fn visible(
    (request, candidate): (&ResolutionRequest<'_>, &ResolutionCandidate),
    inside: bool,
) -> bool {
    if inside || candidate.visibility == Some(Visibility::Public) {
        return true;
    }
    candidate.visibility.is_none() && request.language != "apex"
}

fn inside_class<Cancel>(
    index: &ResolutionIndex,
    (mut owner, class): (Option<&SymbolId>, &SymbolId),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for _ in 0..MAX_OWNER_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else { return Ok(false) };
        if id == class {
            return Ok(true);
        }
        owner = index.parents.get(id);
    }
    Ok(false)
}

fn member_target(candidate: &ResolutionCandidate, current: bool) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: if current { 1.0 } else { STATIC_CONFIDENCE },
        provenance: if current {
            super::CURRENT_CLASS_PROVENANCE
        } else {
            STATIC_PROVENANCE
        },
    }
}
