//! Bounded name fallbacks and calls whose immediate owner proves class scope.

use cartograph_extract::{CallScopeKind, ExtractedCallScopeSite};
use std::collections::HashMap;
use std::mem::size_of;

use super::{
    CandidateMap, ExtractedReference, FileResolutionContext, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceDispatch, ReferenceKind, ReferenceResolution, ResolutionCandidate,
    ResolutionCandidateInsertion, ResolutionIndex, ResolutionRequest, ResolveBudget,
    ResolvedTarget, StageItemFailure, SymbolId, SymbolKind, framework_landmark_candidate,
    push_candidate, usize_to_u64,
};

mod casefold;
mod class_scope;
mod members;
mod name_fallbacks;
mod receiver_sites;
mod scoped_names;

pub(super) use class_scope::scope as call_scope;
pub(super) use receiver_sites::ReceiverSites;
pub(super) use scoped_names::members as scoped_members;

pub(super) const CURRENT_CLASS_PROVENANCE: &str = "native-current-class-call";

#[derive(Default)]
pub(super) struct GenericResolutionIndex {
    /// Kind evidence for immediate callable/type ownership, recorded once per symbol.
    kinds: HashMap<SymbolId, ScopeSymbol>,
    folded_names: CandidateMap,
    scoped_names: scoped_names::ScopedNames,
    call_sites: HashMap<SymbolId, HashMap<(u64, u64), CallScopeKind>>,
}

struct ScopeSymbol {
    kind: SymbolKind,
    static_member: bool,
    constructor: bool,
}

pub(super) fn call_scope_owned_bytes(sites: &Vec<ExtractedCallScopeSite>) -> u64 {
    sites
        .iter()
        .fold(super::vector_capacity_bytes(sites), |bytes, site| {
            bytes.saturating_add(usize_to_u64(site.owner.as_str().len()))
        })
}

/// Add bounded ownership and case-folded lookup evidence during the existing symbol pass.
pub(super) fn index_symbol(
    index: &mut GenericResolutionIndex,
    insertion: ResolutionCandidateInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let symbol = insertion.symbol;
    if (class_language(insertion.language)
        || casefold::case_insensitive_language(insertion.language))
        && (type_kind(symbol.kind) || callable_kind(symbol.kind))
    {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(SymbolId, ScopeSymbol)>()))
                .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len())),
        )?;
        index.kinds.try_reserve(1).map_err(|_| StageItemFailure)?;
        index.kinds.insert(
            symbol.input.symbol_id.clone(),
            ScopeSymbol {
                kind: symbol.kind,
                static_member: symbol.execution.static_member,
                constructor: symbol.declaration_syntax == super::DeclarationSyntax::DartConstructor,
            },
        );
    }
    if class_language(insertion.language) || casefold::case_insensitive_language(insertion.language)
    {
        scoped_names::insert(&mut index.scoped_names, insertion, budget)?;
    }
    casefold::index_names(&mut index.folded_names, insertion, budget)
}

fn class_language(language: &str) -> bool {
    matches!(
        language,
        "javascript"
            | "jsx"
            | "typescript"
            | "tsx"
            | "arkts"
            | "java"
            | "kotlin"
            | "csharp"
            | "cpp"
            | "cuda"
            | "swift"
            | "ruby"
            | "python"
            | "apex"
            | "dart"
            | "groovy"
            | "scala"
            | "astro"
            | "svelte"
            | "vue"
    )
}

fn implicit_receiver_language(language: &str) -> bool {
    matches!(
        language,
        "java" | "kotlin" | "csharp" | "cpp" | "cuda" | "swift" | "ruby"
    )
}

fn type_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Module
    )
}

fn callable_kind(kind: SymbolKind) -> bool {
    matches!(kind, SymbolKind::Function | SymbolKind::Method)
}

fn owning_class<'a>(index: &'a ResolutionIndex, owner: &SymbolId) -> Option<&'a SymbolId> {
    if !index
        .languages
        .generic
        .kinds
        .get(owner)
        .is_some_and(|symbol| callable_kind(symbol.kind))
    {
        return None;
    }
    let parent = index.parents.get(owner)?;
    index
        .languages
        .generic
        .kinds
        .get(parent)
        .is_some_and(|symbol| type_kind(symbol.kind))
        .then_some(parent)
}

pub(super) fn current_instance_class<'index>(
    index: &'index ResolutionIndex,
    reference: &ExtractedReference,
) -> Option<&'index SymbolId> {
    if !class_scope::reference_proven(index, reference) {
        return None;
    }
    let owner = reference.owner.as_ref()?;
    if index.languages.generic.kinds.get(owner)?.static_member {
        return None;
    }
    owning_class(index, owner)
}

/// A receiverless call in these languages searches the immediate class scope.
fn implicit_member_call(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    if !implicit_member_eligible(request, candidate)
        || request.name.contains(['.', ':'])
        || !class_scope::proven_call(index, request)
    {
        return false;
    }
    request
        .owner
        .and_then(|owner| owning_class(index, owner))
        .is_some_and(|class| candidate.parent_symbol_id.as_ref() == Some(class))
        && request
            .owner
            .is_some_and(|owner| compatible_member(index, (owner, &candidate.symbol_id), request))
}

/// Only static calls to callable, non-framework members have an implicit receiver.
fn implicit_member_eligible(
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    request.kind == ReferenceKind::Calls
        && request.dispatch == ReferenceDispatch::Static
        && implicit_receiver_language(request.language)
        && callable_kind(candidate.kind)
        && !framework_landmark_candidate(candidate)
}

/// Owner equality is meaningful for a call, after nearer lexical declarations
/// have been searched; a bare JavaScript/Python method name does not mean `self`.
pub(super) fn recursive_owner_call(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    request.kind == ReferenceKind::Calls
        && request.dispatch == ReferenceDispatch::Static
        && class_scope::proven_call(index, request)
        // Python locals are not retained as binding facts. A same-named local
        // assignment could shadow the function, so owner equality is insufficient.
        && request.language != "python"
        && callable_kind(candidate.kind)
        && (candidate.parent_symbol_id.as_ref().is_none_or(|parent| {
            !index
                .languages.generic
                .kinds
                .get(parent)
                .is_some_and(|symbol| type_kind(symbol.kind))
        }) || implicit_member_call(index, request, candidate))
}

use members::compatible as compatible_member;

fn explicit_member_compatible(
    index: &ResolutionIndex,
    owner: &SymbolId,
    target: &SymbolId,
) -> bool {
    let Some(owner) = index.languages.generic.kinds.get(owner) else {
        return false;
    };
    let Some(target) = index.languages.generic.kinds.get(target) else {
        return false;
    };
    owner.static_member == target.static_member
}

fn receiver_member<'a>(language: &str, name: &'a str) -> Option<&'a str> {
    let member = if language == "swift" {
        name.strip_prefix("self.")
    } else if matches!(
        language,
        "javascript"
            | "jsx"
            | "typescript"
            | "tsx"
            | "arkts"
            | "java"
            | "kotlin"
            | "csharp"
            | "cpp"
            | "cuda"
            | "apex"
            | "groovy"
            | "scala"
            | "astro"
            | "svelte"
            | "vue"
    ) {
        name.strip_prefix("this.")
    } else {
        None
    }?;
    (!member.is_empty() && !member.contains(['.', ':'])).then_some(member)
}

/// Resolve an explicit current-class receiver only within the immediate
/// callable's type. Missing/overloaded members never widen to project names.
pub(super) fn resolve_receiver<Cancel>(
    index: &ResolutionIndex,
    (context, reference): (&FileResolutionContext<'_>, &ExtractedReference),
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if reference.kind != ReferenceKind::Calls {
        return Ok(None);
    }
    let name = context
        .current_receivers
        .lookup(reference)
        .unwrap_or(&reference.name);
    let Some(member) = receiver_member(&context.identity.language, name) else {
        return Ok(None);
    };
    if !class_scope::reference_proven(index, reference)
        && !context.current_receivers.proven(reference)
    {
        return Ok(None);
    }
    let Some(class) = reference
        .owner
        .as_ref()
        .and_then(|owner| owning_class(index, owner))
    else {
        return Ok(None);
    };
    let candidate = select_unique_candidate(
        scoped_names::members(index, class, member),
        |candidate| !candidate.augmentation && !framework_landmark_candidate(candidate),
        cancelled,
    )?;
    let candidate = candidate.filter(|candidate| {
        callable_kind(candidate.kind)
            && reference
                .owner
                .as_ref()
                .is_some_and(|owner| explicit_member_compatible(index, owner, &candidate.symbol_id))
    });
    Ok(candidate.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 1.0,
            provenance: CURRENT_CLASS_PROVENANCE,
        })
    }))
}

pub(super) use casefold::{
    resolve as resolve_casefold_project, resolve_local as resolve_casefold_local,
};
pub(super) use class_scope::{index_calls, resolve as resolve_class_scope};
pub(super) use members::refine as refine_member;
pub(super) use name_fallbacks::resolve_proximity;

/// A weak fallback cannot prefer one implementation over another namespace's
/// declaration: all distinct eligible targets must compete equally.
fn select_unique_candidate<'a, Candidates, Eligible, Cancel>(
    candidates: Candidates,
    mut eligible: Eligible,
    cancelled: &mut Cancel,
) -> Result<Option<&'a ResolutionCandidate>, StageItemFailure>
where
    Candidates: IntoIterator<Item = &'a ResolutionCandidate>,
    Eligible: FnMut(&ResolutionCandidate) -> bool,
    Cancel: FnMut() -> bool,
{
    let mut selected = None;
    let mut total = 0_usize;
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if eligible(candidate) {
            total = total.saturating_add(1);
            selected = Some(candidate);
        }
    }
    Ok(if total == 1 { selected } else { None })
}

pub(super) fn resolve_name_fallback<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::Calls || request.dispatch != ReferenceDispatch::Static {
        return Ok(None);
    }
    name_fallbacks::resolve_suffix(index, request, cancelled)
}
