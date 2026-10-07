//! Literal module dependencies target files independently of imported names.

use super::{
    FileId, IMPORT_BINDING_CONFIDENCE, ModuleResolutionRequest, ReferenceKind, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolKind, javascript_family_name,
    normalize_joined_project_path, normalize_relative_module_path, resolution_languages_compatible,
    resolve_module_file,
};

const PROVENANCE: &str = "native-module-file-path";

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    if request.kind != ReferenceKind::Imports || request.owner.is_some() {
        return Ok(None);
    }
    let Some(path) = literal_path(request) else {
        return Ok(None);
    };
    let file = match index.modules.exact.get(&path) {
        Some(files) => match files.as_slice() {
            [file] if compatible_file(index, (request.language, file)) => Some(file),
            _ => None,
        },
        None => resolve_module_file(
            &index.modules,
            ModuleResolutionRequest {
                importing_path: request.file_path,
                importing_language: request.language,
                specifier: request.name,
            },
        ),
    };
    let Some(file) = file else {
        return Ok(None);
    };
    let symbol = index.file_symbols.get(file).ok_or(StageItemFailure)?;
    Ok(Some(ResolvedTarget {
        symbol_id: symbol.clone(),
        kind: SymbolKind::File,
        confidence: IMPORT_BINDING_CONFIDENCE,
        provenance: PROVENANCE,
    }))
}

fn literal_path(request: &ResolutionRequest<'_>) -> Option<String> {
    if javascript_family_name(request.language) {
        return normalize_relative_module_path(request.file_path, request.name);
    }
    if request.language != "dart"
        || request.name.is_empty()
        || request.name.starts_with('/')
        || request.name.contains([':', '\\', '\0'])
    {
        return None;
    }
    normalize_joined_project_path(request.file_path, request.name)
}

fn compatible_file(index: &ResolutionIndex, (language, file): (&str, &FileId)) -> bool {
    index.modules.files.get(file).is_some_and(|target| {
        resolution_languages_compatible(language, &target.language)
            || (javascript_family_name(language)
                && matches!(target.language.as_str(), "css" | "json"))
    })
}
