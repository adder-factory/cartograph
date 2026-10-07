use super::super::{
    ProjectCandidateInput, ReferenceDispatch, ReferenceKind, ResolutionCandidate, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolKind, Visibility,
    is_project_candidate, javascript_intrinsic_reference, project_source_context,
    python_intrinsic_reference, reference_kind_candidate, resolution_candidates_for_file,
};
use super::select_unique_candidate;

const PROXIMITY_PROVENANCE: &str = "native-path-proximity-fallback";
const SUFFIX_PROVENANCE: &str = "native-qualified-suffix-fallback";

#[derive(Clone, Copy)]
struct FallbackQuery<'a, 'request> {
    index: &'a ResolutionIndex,
    request: &'a ResolutionRequest<'request>,
    candidate: &'a ResolutionCandidate,
}

fn project_eligible(query: FallbackQuery<'_, '_>) -> bool {
    let Some(source) = query.index.modules.files.get(query.request.file_id) else {
        return false;
    };
    is_project_candidate(ProjectCandidateInput {
        modules: &query.index.modules,
        source,
        source_file_id: query.request.file_id,
        reference_name: &query.candidate.qualified_name,
        dynamic_dispatch: false,
        rust_local_import: false,
        candidate: query.candidate,
    }) && reference_kind_candidate(query.request.kind, query.candidate)
}

fn fallback_target(candidate: &ResolutionCandidate, provenance: &'static str) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: if provenance == PROXIMITY_PROVENANCE {
            0.7
        } else {
            0.85
        },
        provenance,
    }
}

fn suffix_language(language: &str) -> bool {
    matches!(language, "cpp" | "cuda")
}

/// Compare complete segments so `Bar::run` cannot match `FooBar::run`.
fn segment_suffix(qualified: &str, suffix: &str) -> bool {
    let mut qualified = qualified.rsplit([':', '.']).filter(|part| !part.is_empty());
    suffix
        .rsplit([':', '.'])
        .filter(|part| !part.is_empty())
        .all(|part| qualified.next() == Some(part))
}

fn suffix_candidate_eligible(query: FallbackQuery<'_, '_>) -> bool {
    let FallbackQuery {
        index: _,
        request,
        candidate,
    } = query;
    segment_suffix(&candidate.qualified_name, request.name)
        && candidate.visibility != Some(Visibility::Private)
        && candidate.visibility != Some(Visibility::Protected)
        && matches!(candidate.kind, SymbolKind::Function | SymbolKind::Method)
        && reference_kind_candidate(request.kind, candidate)
        && if &candidate.file_id == request.file_id {
            candidate.qualified_name != request.name
                && !candidate.augmentation
                && (candidate.export.exported || candidate.visibility == Some(Visibility::Public))
        } else {
            project_eligible(query)
        }
}

pub(super) fn resolve_suffix<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !suffix_language(request.language)
        || !request.name.contains("::")
        || request.name.starts_with("::")
        || !super::class_scope::proven_call(index, request)
    {
        return Ok(None);
    }
    if index.candidates.contains_key(request.name) {
        return Ok(None);
    }
    let Some(qualifier) = request.name.split("::").find(|part| !part.is_empty()) else {
        return Ok(None);
    };
    let Some(name) = request
        .name
        .rsplit([':', '.'])
        .next()
        .filter(|name| !name.is_empty())
    else {
        return Ok(None);
    };
    let Some(bucket) = index.candidates.get(name) else {
        return Ok(None);
    };
    let candidate = select_unique_candidate(
        bucket.iter(),
        |candidate| {
            suffix_candidate_eligible(FallbackQuery {
                index,
                request,
                candidate,
            })
        },
        cancelled,
    )?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    if !qualifier_allows_suffix(index, (qualifier, candidate), cancelled)? {
        return Ok(None);
    }
    Ok(Some(fallback_target(candidate, SUFFIX_PROVENANCE)))
}

fn qualifier_allows_suffix<Cancel>(
    index: &ResolutionIndex,
    (qualifier, candidate): (&str, &ResolutionCandidate),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(declarations) = index.candidates.get(qualifier) else {
        return Ok(true);
    };
    for declaration in declarations.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !matches!(
            declaration.kind,
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Namespace | SymbolKind::Module
        ) || !candidate_container(index, (declaration, candidate), cancelled)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn candidate_container<Cancel>(
    index: &ResolutionIndex,
    (declaration, candidate): (&ResolutionCandidate, &ResolutionCandidate),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut scope = candidate.parent_symbol_id.as_ref();
    for _ in 0..cartograph_extract::MAXIMUM_AST_DEPTH {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(owner) = scope else {
            return Ok(false);
        };
        if owner == &declaration.symbol_id {
            return Ok(true);
        }
        scope = index.parents.get(owner);
    }
    Ok(false)
}

#[derive(Default)]
struct NearestCandidate<'a> {
    best: Option<&'a ResolutionCandidate>,
    depth: usize,
    tied: bool,
}

impl<'a> NearestCandidate<'a> {
    fn observe(&mut self, candidate: &'a ResolutionCandidate, depth: usize) {
        if depth > self.depth {
            self.best = Some(candidate);
            self.depth = depth;
            self.tied = false;
        } else if depth == self.depth {
            self.tied = true;
        }
    }

    fn unique(self) -> Option<&'a ResolutionCandidate> {
        if self.tied { None } else { self.best }
    }
}

fn proximity_allowed(request: &ResolutionRequest<'_>) -> bool {
    request.kind == ReferenceKind::Calls
        && request.language != "rust"
        && request.dispatch == ReferenceDispatch::Static
        && !request.name.contains([':', '.'])
        && !javascript_intrinsic_reference(request)
        && !python_intrinsic_reference(request)
}

/// Only existing externally visible, scope-compatible free functions compete.
/// Directory depth breaks a tie at weaker confidence; an equal best depth abstains.
pub(in super::super) fn resolve_proximity<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !proximity_allowed(request) {
        return Ok(None);
    }
    if !super::class_scope::proven_call(index, request) {
        return Ok(None);
    }
    if !resolution_candidates_for_file(index, request.name, request.file_id).is_empty() {
        return Ok(None);
    }
    let source = project_source_context(index, request)?;
    let Some(bucket) = index.candidates.get(request.name) else {
        return Ok(None);
    };
    let mut nearest = NearestCandidate::default();
    for candidate in bucket.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind != SymbolKind::Function
            || !candidate.top_level
            || !project_eligible(FallbackQuery {
                index,
                request,
                candidate,
            })
        {
            continue;
        }
        let target = index
            .modules
            .files
            .get(&candidate.file_id)
            .ok_or(StageItemFailure)?;
        let depth = source
            .directory
            .split('/')
            .filter(|part| !part.is_empty())
            .zip(target.directory.split('/').filter(|part| !part.is_empty()))
            .take_while(|(source, target)| source == target)
            .count();
        nearest.observe(candidate, depth);
    }
    Ok(nearest
        .unique()
        .map(|candidate| fallback_target(candidate, PROXIMITY_PROVENANCE)))
}
