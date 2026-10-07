//! A default component import denotes the unique exported component, while
//! namespace and named module declarations retain the existing file target.
use super::{
    ModuleResolutionRequest, ResolutionIndex, ResolutionRequest, ResolvedTarget, StageItemFailure,
    SymbolKind, import_binding_target, resolve_module_file, select_candidate,
};

pub(super) const PROVENANCE: &str = "native-component-default-import";

pub(super) fn module_target<Cancel>(
    index: &ResolutionIndex,
    (request, default_only): (&ResolutionRequest<'_>, bool),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !default_only
        || !["~/", "@/", "$lib/"]
            .iter()
            .any(|prefix| request.name.starts_with(prefix))
    {
        return Ok(None);
    }
    let Some(file) = resolve_module_file(
        &index.modules,
        ModuleResolutionRequest {
            importing_path: request.file_path,
            specifier: request.name,
            importing_language: request.language,
        },
    ) else {
        return Ok(None);
    };
    if index
        .modules
        .files
        .get(file)
        .is_none_or(|file| !matches!(file.language.as_str(), "vue" | "svelte"))
    {
        return Ok(None);
    }
    let candidates = index
        .default_exports
        .get(file)
        .map_or(&[][..], Vec::as_slice);
    let target = select_candidate(
        candidates,
        |candidate| candidate.kind == SymbolKind::Component && &candidate.file_id == file,
        cancelled,
    )?;
    Ok(target.map(|candidate| ResolvedTarget {
        provenance: PROVENANCE,
        ..import_binding_target(candidate)
    }))
}
