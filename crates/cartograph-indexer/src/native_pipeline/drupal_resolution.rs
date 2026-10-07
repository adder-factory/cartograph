//! Exact Drupal service and hook resources; no short class-name guesses.

mod classes;
mod services;

pub(super) use classes::ClassIndex;
pub(super) use services::ServiceIndex;

use super::{
    DRUPAL_TAG_CONSUMES_PROVENANCE, DRUPAL_TAG_PROVIDES_PROVENANCE,
    FRAMEWORK_CONVENTION_CONFIDENCE, NativeFileFacts, ReferenceKind, ReferenceResolution,
    ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest, ResolvedTarget,
    StageItemFailure, SymbolId, SymbolKind,
    drupal_tags::{self, HubQuery},
    php_resolution::{self, PhpExactLookup, PhpExactRequest},
    select_candidate,
};

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    classes::index_file(target, file, cancelled)?;
    services::index_file(target, file, cancelled)
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::References {
        return Ok(None);
    }
    if request.language == "yaml"
        && services_path(request.file_path)
        && index.frameworks.drupal_classes.contains(request)
    {
        return class(index, request, cancelled);
    }
    if let Some(resolution) = tag(index, request, cancelled)? {
        return Ok(Some(resolution));
    }
    if request.language == "yaml" && services_path(request.file_path) {
        return services::resolve(index, request, cancelled);
    }
    if request.language != "php" || !request.name.contains("::drupal-hook:") {
        return Ok(None);
    }
    Ok(resource(index, request, cancelled)?.map(|candidate| {
        ReferenceResolution::resolved(target(
            (&candidate.symbol_id, candidate.kind),
            "framework-drupal-resource",
        ))
    }))
}

fn tag<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "yaml" || !services_path(request.file_path) {
        return Ok(None);
    }
    let Some((path, role)) = request.name.split_once("::drupal-service-tag-") else {
        return Ok(None);
    };
    let Some((role, tag)) = role.split_once("::").filter(|_| path == request.file_path) else {
        return Ok(None);
    };
    let provenance = match role {
        "provider" => DRUPAL_TAG_PROVIDES_PROVENANCE,
        "consumer" => DRUPAL_TAG_CONSUMES_PROVENANCE,
        _ => return Ok(None),
    };
    Ok(
        drupal_tags::hub(HubQuery { index, path, tag }, cancelled)?.map(|candidate| {
            ReferenceResolution::resolved(target(
                (&candidate.symbol_id, candidate.kind),
                provenance,
            ))
        }),
    )
}

fn class<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let name = request.name.trim_start_matches('\\');
    let lookup = name.rsplit_once('\\').map_or_else(
        || format!("class::{name}"),
        |(namespace, name)| format!("class::{namespace}::{name}"),
    );
    let Some(lookup) = PhpExactLookup::parse(&lookup) else {
        return Ok(None);
    };
    let resolution = php_resolution::resolve_exact(
        index,
        PhpExactRequest {
            file_id: request.file_id,
            caller_class: None,
            lookup,
        },
        cancelled,
    )?;
    Ok(Some(resolution))
}

pub(super) fn services_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".services.yml") || lower.ends_with(".services.yaml")
}

fn resource<'index, Cancel>(
    index: &'index ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(candidates) = index.candidates.get(request.name) else {
        return Ok(None);
    };
    select_candidate(
        candidates.iter(),
        |candidate| candidate.kind == SymbolKind::Resource && candidate.file_id == *request.file_id,
        cancelled,
    )
}

fn target(candidate: (&SymbolId, SymbolKind), provenance: &'static str) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.0.clone(),
        kind: candidate.1,
        confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
        provenance,
    }
}
