//! The remaining reference tiers preserve the native dispatch order.

use super::{
    DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE, ReferenceResolution, ResolutionIndex,
    ResolutionRequest, StageItemFailure, UNRESOLVED_IMPORT_PROVENANCE, explicit_edge_resolution,
    reference_tiers, resolve_import_or_project_reference, resolve_rust_qualified_path,
    rust_self_has_local_nominal,
};

pub(super) fn resolve_remaining<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(resolution) = reference_tiers::resolve(index, request, cancelled)? {
        return Ok(resolution);
    }
    if request.import_bindings.fallback_blocked {
        return Ok(ReferenceResolution::unresolved(
            UNRESOLVED_IMPORT_PROVENANCE,
        ));
    }
    if rust_self_has_local_nominal(index, request, cancelled)? {
        return Ok(ReferenceResolution::unresolved(
            DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE,
        ));
    }
    if let Some(resolution) = explicit_edge_resolution::resolve(index, request, cancelled)? {
        return Ok(resolution);
    }
    if let Some(target) = resolve_rust_qualified_path(index, request, cancelled)? {
        return Ok(ReferenceResolution::resolved(target));
    }
    resolve_import_or_project_reference(index, request, cancelled)
}
