//! Go namespace bindings whose full import path is represented by the indexed
//! package directory (GOPATH-style layouts). Module-prefix stripping requires
//! an admitted go.mod fact and is deliberately left unsupported here.
use super::{
    ExtractedImportBinding, ImportBindingKind, ReferenceResolution, ResolutionCandidate,
    ResolutionCandidateBucket, ResolutionIndex, ResolutionRequest, ResolvedTarget,
    StageItemFailure, SymbolKind, reference_kind_candidate, resolve_lexical, select_candidate,
};

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "go" {
        return Ok(None);
    }
    let Some((qualifier, name)) = request.name.split_once('.') else {
        return Ok(None);
    };
    let binding = match import_binding((request, qualifier), cancelled)? {
        BindingMatch::Absent | BindingMatch::Ambiguous => return Ok(None),
        BindingMatch::Unique(binding) => binding,
    };
    let qualifier_request = ResolutionRequest {
        name: qualifier,
        kind: super::ReferenceKind::References,
        ..*request
    };
    if resolve_lexical(index, &qualifier_request, cancelled)?
        .is_some_and(|target| target.kind != SymbolKind::Import)
    {
        return Ok(None);
    }
    if !binding
        .module_specifier
        .split('/')
        .next()
        .is_some_and(|root| root.contains('.'))
    {
        return Ok(None);
    }
    let candidates = index
        .candidates
        .get(name)
        .map_or(&[] as &[_], ResolutionCandidateBucket::as_slice);
    let candidate = select_candidate(
        candidates,
        |candidate| {
            candidate.top_level
                && candidate.export.exported
                && candidate.qualified_name == name
                && reference_kind_candidate(request.kind, candidate)
                && package_matches(index, candidate, binding)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 1.0,
            provenance: "native-go-import-path",
        })
    }))
}

enum BindingMatch<'a> {
    Absent,
    Ambiguous,
    Unique(&'a ExtractedImportBinding),
}

fn import_binding<'a, Cancel>(
    query: (&ResolutionRequest<'a>, &str),
    cancelled: &mut Cancel,
) -> Result<BindingMatch<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, qualifier) = query;
    let mut binding = None;
    for candidate in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind != ImportBindingKind::Namespace || candidate.local_name != qualifier {
            continue;
        }
        if binding.is_some() {
            return Ok(BindingMatch::Ambiguous);
        }
        binding = Some(candidate);
    }
    Ok(binding.map_or(BindingMatch::Absent, BindingMatch::Unique))
}

fn package_matches(
    index: &ResolutionIndex,
    candidate: &ResolutionCandidate,
    binding: &ExtractedImportBinding,
) -> bool {
    let Some(file) = index.modules.files.get(&candidate.file_id) else {
        return false;
    };
    let basename = binding.module_specifier.rsplit('/').next();
    let name_agrees = basename != Some(binding.local_name.as_str())
        || file.package.as_deref() == Some(binding.local_name.as_str());
    file.language == "go"
        && file.directory == binding.module_specifier
        && file.package.is_some()
        && name_agrees
}
