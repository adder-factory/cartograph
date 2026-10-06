//! Follow a lexical JVM type head only through its own nested type identities.

use cartograph_domain::{ReferenceKind, SymbolId, Visibility};

use super::{
    ReferenceResolution, ResolutionCandidate, ResolutionIndex, ResolutionRequest, ResolvedTarget,
    StageItemFailure, UNRESOLVED_PROVENANCE, jvm_resolution, nominal_scope_resolution,
    resolution_candidates_for_file,
};

const MAX_NESTED_TYPES: usize = 64;

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some((head, suffix)) = request.name.split_once('.') else {
        return Ok(None);
    };
    let head_request = ResolutionRequest {
        name: head,
        kind: ReferenceKind::TypeOf,
        ..*request
    };
    let Some(head) = nominal_scope_resolution::resolve(index, &head_request, cancelled)? else {
        return Ok(None);
    };
    let Some(mut parent) = head.target else {
        return Ok(Some(head));
    };
    for (position, segment) in suffix.split('.').enumerate() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if position >= MAX_NESTED_TYPES || !jvm_resolution::nominal_type(parent.kind) {
            return Ok(Some(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)));
        }
        let query = NestedQuery {
            index,
            request,
            parent: &parent.symbol_id,
            segment,
        };
        let Some(candidate) = nested_type(&query, cancelled)? else {
            return Ok(Some(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)));
        };
        parent = ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 0.95,
            provenance: jvm_resolution::QUALIFIED_PROVENANCE,
        };
    }
    Ok(Some(ReferenceResolution::resolved(parent)))
}

struct NestedQuery<'index, 'request, 'name> {
    index: &'index ResolutionIndex,
    request: &'index ResolutionRequest<'request>,
    parent: &'name SymbolId,
    segment: &'name str,
}

fn nested_type<'index, Cancel>(
    query: &NestedQuery<'index, '_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut selected = None;
    for candidate in
        resolution_candidates_for_file(query.index, query.segment, query.request.file_id)
    {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.parent_symbol_id.as_ref() != Some(query.parent) {
            continue;
        }
        if jvm_resolution::callable_competitor(query.request, candidate.kind)
            || jvm_resolution::constructor_target_abstains(
                query.index,
                (query.request, &candidate.symbol_id, candidate.kind),
            )
        {
            return Ok(None);
        }
        if !jvm_resolution::nominal_type(candidate.kind) {
            continue;
        }
        if selected.is_some() || candidate.visibility == Some(Visibility::Private) {
            return Ok(None);
        }
        selected = Some(candidate);
    }
    Ok(selected)
}
