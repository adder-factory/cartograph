//! Bind a convention route to the public method in its own named controller.

use super::{
    ReferenceKind, ReferenceResolution, ResolutionIndex, ResolutionRequest, StageItemFailure,
    SymbolKind, Visibility, qualtype_resolution::Selection, resolution_candidates_for_file,
};

pub(super) const PROVENANCE: &str = "framework-codeigniter-controller-route";

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "php" || request.kind != ReferenceKind::Calls {
        return Ok(None);
    }
    let Some(controller) = request
        .file_path
        .strip_prefix("application/controllers/")
        .and_then(|path| path.strip_suffix(".php"))
        .and_then(|path| path.rsplit('/').next())
    else {
        return Ok(None);
    };
    let Some(owner) = request
        .owner
        .and_then(|owner| index.qualtype.owners.get(owner))
    else {
        return Ok(None);
    };
    if owner.kind != SymbolKind::Route
        || !owner.name.starts_with(request.file_path)
        || !owner.name[request.file_path.len()..].starts_with("::any::/")
    {
        return Ok(None);
    }
    let mut selected = Selection::default();
    for candidate in resolution_candidates_for_file(index, request.name, request.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind != SymbolKind::Method || candidate.visibility != Some(Visibility::Public)
        {
            continue;
        }
        let parent = candidate
            .parent_symbol_id
            .as_ref()
            .and_then(|parent| index.qualtype.owners.get(parent));
        if parent
            .is_some_and(|parent| parent.kind == SymbolKind::Class && parent.name == controller)
        {
            selected.retain(candidate);
        }
    }
    Ok(selected.resolution(PROVENANCE, 0.9))
}
