//! Route controller conventions never discard an explicit PHP namespace.

use super::super::{
    ImportReferenceSite, ImportResolution, ImportResolutionRequest, php_route_source,
    project_source_context, resolve_import,
};
use super::{
    ExactQuery, Intent, ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolKind, UNRESOLVED_PROVENANCE,
    Visibility, php_candidate, select_candidate, synthesized_key,
};

pub(in crate::native_pipeline) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(request.language, "php" | "yaml")
        || !matches!(
            request.kind,
            ReferenceKind::Calls | ReferenceKind::References
        )
    {
        return Ok(None);
    }
    if !php_route_source(project_source_context(index, request)?) {
        return Ok(None);
    }
    let Some((owner, member)) = request.name.rsplit_once("::") else {
        return Ok(None);
    };
    let Some((class_name, qualified)) = class_key(owner) else {
        return Ok(None);
    };
    if let Some(resolution) = exact_import(index, (request, &class_name), cancelled)? {
        return Ok(Some(resolution));
    }
    let Some(class) = route_class(index, (request, &class_name, qualified), cancelled)? else {
        return Ok(Some(unresolved()));
    };
    let Some(key) = synthesized_key(&[&class.qualified_name, "::", member]) else {
        return Ok(Some(unresolved()));
    };
    let exact = ExactQuery {
        index,
        file_id: request.file_id,
        caller_class: None,
        intent: Intent::Member,
        key: &key,
    };
    if let Some(candidate) = exact.candidate(cancelled)? {
        return Ok(Some(ReferenceResolution::resolved(target(
            candidate, qualified,
        ))));
    }
    if exact.declared(cancelled)? {
        return Ok(Some(unresolved()));
    }
    let resolution = match super::members::lookup(exact, cancelled)? {
        super::members::MemberMatch::Unique(candidate) => {
            ReferenceResolution::resolved(target(candidate, qualified))
        }
        super::members::MemberMatch::Blocked => unresolved(),
        super::members::MemberMatch::Missing => ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: class.symbol_id.clone(),
            kind: class.kind,
            confidence: 0.75,
            provenance: "framework-php-controller-class-fallback",
        }),
    };
    Ok(Some(resolution))
}

fn exact_import<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, class_name) = query;
    if request.name.starts_with('\\') {
        return Ok(None);
    }
    match imported_class(index, (request, class_name), cancelled)? {
        ImportClassMatch::Missing => return Ok(None),
        ImportClassMatch::Bound(None) => return Ok(Some(unresolved())),
        ImportClassMatch::Bound(Some(_)) => {}
    }
    Ok(
        match resolve_import(
            index,
            ImportResolutionRequest {
                reference: request,
                site: ImportReferenceSite::Usage,
            },
            cancelled,
        )? {
            ImportResolution::Resolved(target) => Some(ReferenceResolution::resolved(target)),
            _ => None,
        },
    )
}

fn class_key(owner: &str) -> Option<(String, bool)> {
    let global = owner.starts_with('\\');
    let owner = owner.trim_start_matches('\\');
    if !owner
        .get(owner.len().saturating_sub(10)..)?
        .eq_ignore_ascii_case("Controller")
    {
        return None;
    }
    if owner.contains("::") {
        return Some((synthesized_key(&[owner])?, true));
    }
    let (namespace, class) = owner.rsplit_once('\\').unwrap_or(("", owner));
    let key = if namespace.is_empty() {
        synthesized_key(&[class])?
    } else {
        synthesized_key(&[namespace, "::", class])?
    };
    Some((key, global || !namespace.is_empty()))
}

fn route_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str, bool),
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, key, qualified) = query;
    if qualified {
        return ExactQuery {
            index,
            file_id: request.file_id,
            caller_class: None,
            intent: Intent::Class,
            key,
        }
        .candidate(cancelled)
        .map(|candidate| candidate.filter(|candidate| candidate.kind == SymbolKind::Class));
    }
    match imported_class(index, (request, key), cancelled)? {
        ImportClassMatch::Missing => {}
        ImportClassMatch::Bound(imported) => return Ok(imported),
    }
    let Some(candidates) = index.candidates.get(key) else {
        return Ok(None);
    };
    select_candidate(
        candidates.iter(),
        |candidate| {
            candidate.kind == SymbolKind::Class
                && candidate.visibility != Some(Visibility::Private)
                && php_candidate(index, candidate)
        },
        cancelled,
    )
}

/// A declared PHP alias is authoritative even when it names an external class.
enum ImportClassMatch<'index> {
    Missing,
    Bound(Option<&'index ResolutionCandidate>),
}

fn imported_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<ImportClassMatch<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, owner) = query;
    if request.language != "php" {
        return Ok(ImportClassMatch::Missing);
    }
    let key = match index
        .languages
        .php
        .route_aliases
        .lookup(request.file_id, owner)
    {
        super::route_aliases::AliasMatch::Missing => return Ok(ImportClassMatch::Missing),
        super::route_aliases::AliasMatch::Blocked => return Ok(ImportClassMatch::Bound(None)),
        super::route_aliases::AliasMatch::Target(key) => key,
    };
    Ok(ImportClassMatch::Bound(
        ExactQuery {
            index,
            file_id: request.file_id,
            caller_class: None,
            intent: Intent::Class,
            key,
        }
        .candidate(cancelled)?
        .filter(|candidate| candidate.kind == SymbolKind::Class),
    ))
}

fn target(candidate: &ResolutionCandidate, qualified: bool) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: if qualified { 0.90 } else { 0.80 },
        provenance: if qualified {
            "framework-php-controller-qualified"
        } else {
            "framework-php-controller-convention"
        },
    }
}

fn unresolved() -> ReferenceResolution {
    ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)
}
