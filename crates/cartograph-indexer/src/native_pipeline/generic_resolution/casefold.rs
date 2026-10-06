use super::super::{
    EXACT_LEXICAL_PROVENANCE, EXACT_SAME_FILE_PROVENANCE, ProjectCandidateInput,
    is_project_candidate, project_resolved_target, try_clone_text,
};
use super::{
    CandidateMap, ReferenceDispatch, ReferenceKind, ReferenceResolution, ResolutionCandidate,
    ResolutionCandidateInsertion, ResolutionIndex, ResolutionRequest, ResolveBudget,
    ResolvedTarget, StageItemFailure, callable_kind, push_candidate, scoped_names,
    select_unique_candidate,
};

const CASEFOLD_PROVENANCE: &str = "native-case-insensitive-callable-fallback";

pub(super) fn case_insensitive_language(language: &str) -> bool {
    // Ada/VHDL already canonicalize identifiers in their dedicated extractor
    // and module resolver, whose declaration/body preference stays authoritative.
    matches!(language, "vbnet" | "vb6" | "pascal" | "sql")
}

pub(super) fn index_names(
    candidates: &mut CandidateMap,
    insertion: ResolutionCandidateInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !case_insensitive_language(insertion.language) {
        return Ok(());
    }
    let mut name = try_clone_text(insertion.key)?;
    name.make_ascii_lowercase();
    push_candidate(
        candidates,
        ResolutionCandidateInsertion {
            key: &name,
            ..insertion
        },
        budget,
    )?;
    if insertion.symbol.input.qualified_name != insertion.key {
        let mut qualified = try_clone_text(&insertion.symbol.input.qualified_name)?;
        qualified.make_ascii_lowercase();
        push_candidate(
            candidates,
            ResolutionCandidateInsertion {
                key: &qualified,
                ..insertion
            },
            budget,
        )?;
    }
    Ok(())
}

fn applicable(request: &ResolutionRequest<'_>) -> bool {
    case_insensitive_language(request.language)
        && request.kind == ReferenceKind::Calls
        && request.dispatch == ReferenceDispatch::Static
        && !request.name.contains([':', '.'])
}

fn folded_bucket<'a>(
    index: &'a ResolutionIndex,
    request: &ResolutionRequest<'_>,
) -> Result<Option<&'a super::super::ResolutionCandidateBucket>, StageItemFailure> {
    let mut name = try_clone_text(request.name)?;
    name.make_ascii_lowercase();
    Ok(index.generic.folded_names.get(&name))
}

fn local_target(
    candidate: &ResolutionCandidate,
    request: &ResolutionRequest<'_>,
) -> ResolvedTarget {
    let exact_spelling = candidate.qualified_name.rsplit("::").next() == Some(request.name);
    let provenance = if !exact_spelling {
        CASEFOLD_PROVENANCE
    } else if candidate.parent_symbol_id.is_some() {
        EXACT_LEXICAL_PROVENANCE
    } else {
        EXACT_SAME_FILE_PROVENANCE
    };
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: if exact_spelling { 1.0 } else { 0.5 },
        provenance,
    }
}

/// Case variants compete in their indexed lexical scope, including direct recursion.
pub(in super::super) fn resolve_local<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !applicable(request) || !super::class_scope::proven_call(index, request) {
        return Ok(None);
    }
    let mut scope = request.owner;
    for _ in 0..=index.parents.len().saturating_add(1) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let candidates = scoped_names::candidates(index, request, scope)?;
        if !candidates.is_empty() {
            let candidate = select_unique_candidate(
                candidates,
                |candidate| !candidate.augmentation,
                cancelled,
            )?;
            return Ok(candidate
                .filter(|candidate| callable_kind(candidate.kind))
                .map(|candidate| ReferenceResolution::resolved(local_target(candidate, request))));
        }
        let Some(owner) = scope else {
            return Ok(None);
        };
        scope = index.parents.get(owner);
    }
    Ok(None)
}

pub(in super::super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !applicable(request)
        || request.language == "vbnet" && !super::class_scope::proven_call(index, request)
    {
        return Ok(None);
    }
    let source = index
        .modules
        .files
        .get(request.file_id)
        .ok_or(StageItemFailure)?;
    let Some(bucket) = folded_bucket(index, request)? else {
        return Ok(None);
    };
    let candidate = select_unique_candidate(
        bucket.iter(),
        |candidate| {
            let same_language = index
                .modules
                .files
                .get(&candidate.file_id)
                .is_some_and(|target| target.language == request.language);
            same_language
                && callable_kind(candidate.kind)
                && candidate.top_level
                && is_project_candidate(ProjectCandidateInput {
                    modules: &index.modules,
                    source,
                    source_file_id: request.file_id,
                    reference_name: &candidate.qualified_name,
                    dynamic_dispatch: false,
                    rust_local_import: false,
                    candidate,
                })
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| {
        if candidate.qualified_name == request.name {
            project_resolved_target(candidate)
        } else {
            ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence: 0.5,
                provenance: CASEFOLD_PROVENANCE,
            }
        }
    }))
}
