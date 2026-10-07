//! Play handlers are package/class/member names, not bare method guesses.

use super::{
    ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionIndex, ResolutionRequest,
    ResolvedTarget, StageItemFailure, SymbolKind, UNRESOLVED_PROVENANCE, Visibility,
    select_candidate, try_clone_text,
};

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "yaml"
        || !route_file(request.file_path)
        || request.kind != ReferenceKind::Calls
    {
        return Ok(None);
    }
    let Some((owner, method)) = request.name.rsplit_once('.') else {
        return Ok(None);
    };
    let class = qualified_key(owner)?;
    let Some(declaration) = handler_class(index, &class, cancelled)? else {
        return Ok(Some(unresolved()));
    };
    let candidate = handler_method(index, (declaration, method), cancelled)?;
    Ok(Some(candidate.map_or_else(unresolved, |candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 0.90,
            provenance: "framework-play-qualified-handler",
        })
    })))
}

fn handler_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    class: &str,
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(classes) = index.candidates.get(class) else {
        return Ok(None);
    };
    select_candidate(
        classes.iter(),
        |candidate| {
            candidate.qualified_name == class
                && candidate.kind == SymbolKind::Class
                && matches!(candidate.visibility, None | Some(Visibility::Public))
                && jvm_candidate(index, candidate)
        },
        cancelled,
    )
}

fn handler_method<'index, Cancel>(
    index: &'index ResolutionIndex,
    handler: (&ResolutionCandidate, &str),
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (declaration, name) = handler;
    let Some(key) = index.framework_methods.key(&declaration.symbol_id, name) else {
        return Ok(None);
    };
    let Some(methods) = index.candidates.get(key) else {
        return Ok(None);
    };
    select_candidate(
        methods.iter(),
        |candidate| {
            candidate.parent_symbol_id.as_ref() == Some(&declaration.symbol_id)
                && matches!(candidate.kind, SymbolKind::Method | SymbolKind::Function)
                && matches!(candidate.visibility, None | Some(Visibility::Public))
                && jvm_candidate(index, candidate)
        },
        cancelled,
    )
}

fn route_file(path: &str) -> bool {
    let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
    (directory == "conf" || directory.ends_with("/conf"))
        && (name == "routes" || name.ends_with(".routes"))
}

fn jvm_candidate(index: &ResolutionIndex, candidate: &ResolutionCandidate) -> bool {
    index
        .modules
        .files
        .get(&candidate.file_id)
        .is_some_and(|file| matches!(file.language.as_str(), "java" | "scala"))
}

fn qualified_key(name: &str) -> Result<String, StageItemFailure> {
    let Some((package, class)) = name.rsplit_once('.') else {
        return try_clone_text(name);
    };
    let mut key = try_clone_text(package)?;
    key.try_reserve(2 + class.len())
        .map_err(|_| StageItemFailure)?;
    key.push_str("::");
    key.push_str(class);
    Ok(key)
}

fn unresolved() -> ReferenceResolution {
    ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)
}
