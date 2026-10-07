//! Dispatch the bounded include, import, and module rules after lexical lookup.
use super::{ReferenceResolution, ResolutionIndex, ResolutionRequest, StageItemFailure};

pub(super) fn metadata_binding(language: &str, binding: &super::ExtractedImportBinding) -> bool {
    super::module_call_resolution::metadata_binding(language, binding)
        || super::rust_dependency_paths::metadata_binding(language, binding)
        || super::shell_resolution::metadata_binding(language, binding)
        || super::rust_use_bindings::metadata_binding(language, binding)
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    match request.language {
        "bash" | "zsh" | "fish" | "powershell" => {
            super::shell_resolution::resolve(index, request, cancelled)
        }
        "rust" => super::rust_path_resolution::resolve(index, request, cancelled),
        "go" => super::go_path_resolution::resolve(index, request, cancelled),
        _ => Ok(None),
    }
}
