//! Explicit framework references are authoritative even when unresolved.

use super::{
    ReferenceResolution, ResolutionIndex, ResolutionRequest, StageItemFailure, drupal_resolution,
    php_resolution, play_resolution, salesforce_resolution,
};

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(resolution) = drupal_resolution::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = super::route_bridges::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = super::route_bridges::mybatis_class(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = salesforce_resolution::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = play_resolution::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = super::codeigniter_resources::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    php_resolution::resolve_route(index, request, cancelled)
}
