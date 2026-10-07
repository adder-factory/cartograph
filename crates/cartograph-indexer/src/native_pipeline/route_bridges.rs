//! Typed route/property/XML signals precede general language fallbacks.

use super::{
    HashMap, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind, ReferenceResolution,
    ResolutionCandidate, ResolutionCandidateInsertion, ResolutionIndex, ResolutionRequest,
    ResolveBudget, ResolvedTarget, StageItemFailure, SymbolId, SymbolKind, Visibility,
    select_candidate, size_of, usize_to_u64,
};

mod flutter;
mod mybatis;

const BRIDGE_CONFIDENCE: f32 = 0.90;
const MAPPER_CLASS_PREFIX: &str = "mybatis-class::";
const CLASS_UNRESOLVED_PROVENANCE: &str = "framework-mybatis-configuration-class-unresolved";

pub(super) fn authoritative_lookup(name: &str) -> bool {
    name.starts_with(MAPPER_CLASS_PREFIX)
        || name.starts_with("mybatis-template::")
        || name.starts_with("mybatis-template-local::")
}

#[derive(Default)]
pub(super) struct Owners {
    kinds: HashMap<SymbolId, SymbolKind>,
    templates: mybatis::Templates,
}

pub(super) fn index_symbol(
    owners: &mut Owners,
    insertion: ResolutionCandidateInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let symbol = insertion.symbol;
    mybatis::index_symbol(&mut owners.templates, insertion, budget)?;
    let field = matches!(insertion.language, "java" | "kotlin") && symbol.kind == SymbolKind::Field;
    let route = insertion.language == "dart" && symbol.kind == SymbolKind::Route;
    if !field && !route {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(SymbolId, SymbolKind)>()))
            .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len())),
    )?;
    owners.kinds.try_reserve(1).map_err(|_| StageItemFailure)?;
    owners
        .kinds
        .insert(symbol.input.symbol_id.clone(), symbol.kind);
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language == "dart" {
        return flutter::resolve(index, request, cancelled);
    }
    if let Some(resolved) = mybatis::resolve(index, request, cancelled)? {
        return Ok(Some(resolved));
    }
    let Some((kind, provenance)) = signal(index, request) else {
        return Ok(None);
    };
    let Some(candidates) = index.candidates.get(request.name) else {
        return Ok(None);
    };
    let candidate = select_candidate(
        candidates.iter(),
        |candidate| {
            candidate.qualified_name == request.name
                && candidate.kind == kind
                && property_target(index, candidate)
                && candidate.visibility != Some(Visibility::Private)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: BRIDGE_CONFIDENCE,
            provenance,
        })
    }))
}

fn signal(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
) -> Option<(SymbolKind, &'static str)> {
    if request.kind != ReferenceKind::References {
        return None;
    }
    let owner = request
        .owner
        .and_then(|id| index.frameworks.route_bridges.kinds.get(id));
    match request.language {
        "java" | "kotlin" if owner == Some(&SymbolKind::Field) && request.name.contains('.') => {
            Some((SymbolKind::Constant, "framework-spring-property"))
        }
        _ => None,
    }
}

fn property_target(index: &ResolutionIndex, candidate: &ResolutionCandidate) -> bool {
    index
        .modules
        .files
        .get(&candidate.file_id)
        .is_some_and(|file| file.language == "properties")
}

pub(super) fn mybatis_class<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "xml"
        || request.kind != ReferenceKind::References
        || request.owner.is_some()
    {
        return Ok(None);
    }
    let Some(name) = request.name.strip_prefix(MAPPER_CLASS_PREFIX) else {
        return Ok(None);
    };
    let Some(candidates) = index.candidates.get(name) else {
        return Ok(Some(ReferenceResolution::unresolved(
            CLASS_UNRESOLVED_PROVENANCE,
        )));
    };
    let candidate = select_candidate(
        candidates.iter(),
        |candidate| {
            candidate.qualified_name == name
                && matches!(candidate.kind, SymbolKind::Class | SymbolKind::Interface)
                && candidate.visibility != Some(Visibility::Private)
                && index
                    .modules
                    .files
                    .get(&candidate.file_id)
                    .is_some_and(|file| {
                        matches!(file.language.as_str(), "java" | "kotlin" | "scala")
                    })
        },
        cancelled,
    )?;
    Ok(Some(candidate.map_or_else(
        || ReferenceResolution::unresolved(CLASS_UNRESOLVED_PROVENANCE),
        |candidate| {
            ReferenceResolution::resolved(ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence: BRIDGE_CONFIDENCE,
                provenance: "framework-mybatis-configuration-class",
            })
        },
    )))
}
