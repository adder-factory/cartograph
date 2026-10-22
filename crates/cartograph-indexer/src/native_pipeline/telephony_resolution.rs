//! Telephony references never bind external applications or builtins by name.
//! Kamailio route identities require the current file or a literal direct include.
use super::{
    ExtractedImportBinding, FileId, ImportBindingKind, ModuleFileMatch, ReferenceKind,
    ReferenceResolution, ResolutionIndex, ResolutionRequest, ResolvedTarget, StageItemFailure,
    SymbolKind, module_file_match, normalize_include_path, normalize_root_module_path,
};

const UNRESOLVED: &str = "native-kamailio-unresolved";
const PROVENANCE: &str = "native-kamailio-static-config";

pub(super) fn authoritative_lookup(name: &str) -> bool {
    name.starts_with("freeswitch::application::")
        || name.starts_with("freeswitch::module::")
        || name.starts_with("freeswitch::include::")
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language == "xml" && authoritative_lookup(request.name) {
        return resolve_freeswitch(index, request, cancelled).map(Some);
    }
    if request.language != "kamailio" {
        return Ok(None);
    }
    if cancelled() {
        return Err(StageItemFailure);
    }
    if request.kind == ReferenceKind::Imports {
        return Ok(Some(resolve_import(
            index,
            request,
            request.name,
            PROVENANCE,
            UNRESOLVED,
            cancelled,
        )?));
    }
    resolve_route(index, request, cancelled).map(Some)
}

fn resolve_freeswitch<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    if request.kind == ReferenceKind::Imports
        && let Some(specifier) = request.name.strip_prefix("freeswitch::include::")
    {
        return resolve_import(
            index,
            request,
            specifier,
            "native-freeswitch-static-config",
            "native-freeswitch-unresolved",
            cancelled,
        );
    }
    Ok(ReferenceResolution::unresolved(
        "native-freeswitch-external",
    ))
}

fn resolve_route<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(bucket) = index
        .candidates
        .get(request.name)
        .filter(|_| !request.name.starts_with("kamailio::builtin::"))
    else {
        return Ok(ReferenceResolution::unresolved(UNRESOLVED));
    };
    let mut retained = None;
    for candidate in bucket.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind != SymbolKind::Function || candidate.qualified_name != request.name {
            continue;
        }
        if !route_visible(index, request, &candidate.file_id, cancelled)? {
            continue;
        }
        if retained.is_some() {
            return Ok(ReferenceResolution::unresolved(UNRESOLVED));
        }
        retained = Some(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 1.0,
            provenance: PROVENANCE,
        });
    }
    Ok(retained.map_or_else(
        || ReferenceResolution::unresolved(UNRESOLVED),
        ReferenceResolution::resolved,
    ))
}

fn route_visible<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    file: &FileId,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut visible = file == request.file_id;
    for binding in request.import_bindings.bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        visible |= include_file(index, request, binding) == Some(file);
    }
    Ok(visible)
}

fn resolve_import<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    specifier: &str,
    provenance: &'static str,
    unresolved: &'static str,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut file = None;
    // Prefixed XML lookup names differ from the source-visible import name,
    // so select the exact span/specifier from the file's retained bindings.
    for binding in request.import_bindings.bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.span == request.span && binding.module_specifier == specifier {
            file = include_file(index, request, binding);
            break;
        }
    }
    let target = file
        .and_then(|id| index.file_symbols.get(id))
        .map(|id| ResolvedTarget {
            symbol_id: id.clone(),
            kind: SymbolKind::File,
            confidence: 1.0,
            provenance,
        });
    Ok(target.map_or_else(
        || ReferenceResolution::unresolved(unresolved),
        ReferenceResolution::resolved,
    ))
}

/// `FreeSWITCH` includes use the including XML directory. Kamailio first tries
/// runtime CWD, represented by the project root, then the including directory.
/// Absolute, wildcard and macro paths abstain.
fn include_file<'index>(
    index: &'index ResolutionIndex,
    request: &ResolutionRequest<'_>,
    binding: &ExtractedImportBinding,
) -> Option<&'index FileId> {
    let specifier = binding.module_specifier.as_str();
    if binding.kind != ImportBindingKind::IncludeQuoted
        || specifier.contains(['$', '*', '?', '\\', '\0'])
        || specifier.starts_with('/')
        || specifier.as_bytes().get(1) == Some(&b':')
    {
        return None;
    }
    let sibling = normalize_include_path(request.file_path, &binding.module_specifier);
    let paths = if request.language == "xml" {
        [sibling, None]
    } else {
        [normalize_root_module_path(specifier), sibling]
    };
    for path in paths.into_iter().flatten() {
        match module_file_match(
            index.modules.exact.get(&path),
            &index.modules.files,
            request.language,
        ) {
            ModuleFileMatch::Unique(file) => return Some(file),
            ModuleFileMatch::Ambiguous => return None,
            ModuleFileMatch::Missing => {}
        }
    }
    None
}
