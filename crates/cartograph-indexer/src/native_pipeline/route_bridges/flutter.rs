//! Bounded route-widget fallback over written relative Dart imports.
//! The material/router SDK imports carry no project-owned widget evidence.

use super::super::{
    ExtractedImportBinding, ImportReferenceSite, ImportResolutionRequest, ModuleImportQuery,
    ReferenceKind, ReferenceResolution, ResolutionIndex, ResolutionRequest, ResolvedTarget,
    StageItemFailure, SymbolKind, normalize_joined_project_path, resolution_candidates_for_file,
    resolve_module_import, resolve_normalized_module_file,
};

const MAX_ROUTE_IMPORTS: usize = 64;
const IMPORT_FALLBACK_CONFIDENCE: f32 = 0.75;

enum WidgetMatch {
    Skip,
    Opaque,
    Target(ResolvedTarget),
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !route_request(index, request) {
        return Ok(None);
    }
    Ok(
        imported_widgets(index, request, cancelled)?.map(|mut target| {
            target.confidence = IMPORT_FALLBACK_CONFIDENCE;
            target.provenance = "framework-flutter-relative-widget-fallback";
            ReferenceResolution::resolved(target)
        }),
    )
}

fn route_request(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    matches!(
        request.kind,
        ReferenceKind::References | ReferenceKind::Calls
    ) && request
        .owner
        .and_then(|id| index.frameworks.route_bridges.kinds.get(id))
        == Some(&SymbolKind::Route)
        && request.import_bindings.positions.len() <= MAX_ROUTE_IMPORTS
        && !request.name.starts_with('_')
        && resolution_candidates_for_file(index, request.name, request.file_id).is_empty()
}

fn imported_widgets<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let typed = ResolutionRequest {
        kind: ReferenceKind::TypeOf,
        ..*request
    };
    let mut matched: Option<ResolvedTarget> = None;
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        match module_widget(index, (&typed, binding), cancelled)? {
            WidgetMatch::Skip => {}
            WidgetMatch::Opaque => return Ok(None),
            WidgetMatch::Target(target) => {
                if matched
                    .as_ref()
                    .is_some_and(|known| known.symbol_id != target.symbol_id)
                {
                    return Ok(None);
                }
                matched = Some(target);
            }
        }
    }
    Ok(matched)
}

fn module_widget<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &ExtractedImportBinding),
    cancelled: &mut Cancel,
) -> Result<WidgetMatch, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, binding) = query;
    if binding.local_name != "*"
        || binding.imported_name != "*"
        || sdk_import(&binding.module_specifier)
    {
        return Ok(WidgetMatch::Skip);
    }
    let Some(normalized) = relative_module(request.file_path, &binding.module_specifier) else {
        return Ok(WidgetMatch::Opaque);
    };
    let Some(module_file_id) = resolve_normalized_module_file(&index.modules, &normalized, "dart")
    else {
        return Ok(WidgetMatch::Opaque);
    };
    let target = resolve_module_import(
        ModuleImportQuery {
            index,
            import: ImportResolutionRequest {
                reference: request,
                site: ImportReferenceSite::Usage,
            },
            binding,
            imported_name: request.name,
            module_file_id,
        },
        cancelled,
    )?;
    Ok(match target {
        Some(target) if target.kind == SymbolKind::Class => WidgetMatch::Target(target),
        None if resolution_candidates_for_file(index, request.name, module_file_id).is_empty() => {
            WidgetMatch::Skip
        }
        _ => WidgetMatch::Opaque,
    })
}

fn sdk_import(module: &str) -> bool {
    module.starts_with("dart:")
        || module.starts_with("package:flutter/")
        || module.starts_with("package:go_router/")
}

fn relative_module(path: &str, module: &str) -> Option<String> {
    if !std::path::Path::new(module)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dart"))
        || module.starts_with('/')
        || module.contains([':', '\\', '\0'])
    {
        return None;
    }
    normalize_joined_project_path(path, module)
}
