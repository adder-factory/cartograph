//! The merged track tiers retain their precedence before explicit path/import
//! and project fallback resolution.
use super::{
    ReferenceResolution, ResolutionIndex, ResolutionRequest, StageItemFailure,
    codeigniter_resolution, csharp_constructors, declaration_resolution, enum_resolution,
    generic_resolution, javascript_member_resolution, module_call_resolution, qualtype_resolution,
    resolve_lexical, resource_resolution,
};

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if let Some(resolution) = javascript_member_resolution::guard(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = generic_resolution::resolve_casefold_local(index, request, cancelled)?
    {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = generic_resolution::resolve_class_scope(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = codeigniter_resolution::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = enum_resolution::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = csharp_constructors::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(target) = resource_resolution::resolve(index, request, cancelled)? {
        return Ok(Some(ReferenceResolution::resolved(target)));
    }
    if let Some(resolution) = module_call_resolution::qualified(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(resolution) = super::ocaml_module_resolution::resolve(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if let Some(target) = resolve_lexical(index, request, cancelled)? {
        return declaration_resolution::prefer_definition(index, (request, target), cancelled)
            .map(Some);
    }
    qualtype_resolution::resolve(index, request, cancelled)
}
