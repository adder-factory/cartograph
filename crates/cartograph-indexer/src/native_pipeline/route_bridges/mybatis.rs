//! Mapper namespaces and template declaration roles precede name conventions.

use super::super::{
    EXACT_SAME_FILE_CONFIDENCE, EXACT_SAME_FILE_PROVENANCE, HashMap, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionCandidateInsertion,
    ResolutionIndex, ResolutionRequest, ResolveBudget, ResolvedTarget, StageItemFailure, SymbolId,
    select_candidate, size_of, try_clone_text, usize_to_u64,
};

const TEMPLATE_PREFIX: &str = "mybatis-template::";
const LOCAL_PREFIX: &str = "mybatis-template-local::";
const TEMPLATE_CONFIDENCE: f32 = 0.90;
const UNPROVEN_CLASS_LOOKUP: &str = "cartograph.mybatis-unproven-class";
const UNPROVEN_TEMPLATE_LOOKUP: &str = "cartograph.mybatis-unproven-template";
const UNSUPPORTED_PROVENANCE: &str = "framework-mybatis-unsupported-qualification";
const TEMPLATE_UNRESOLVED_PROVENANCE: &str = "framework-mybatis-template-unresolved";

#[derive(Default)]
pub(super) struct Templates {
    namespaces: HashMap<SymbolId, String>,
    roles: HashMap<SymbolId, &'static str>,
}

pub(super) fn index_symbol(
    templates: &mut Templates,
    insertion: ResolutionCandidateInsertion<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let symbol = insertion.symbol;
    if insertion.language != "xml" {
        return Ok(());
    }
    let namespace = (symbol.kind == super::SymbolKind::Namespace)
        .then_some(symbol.input.qualified_name.as_str());
    let role = declaration_role(&symbol.body_search_text);
    if namespace.is_none() && role.is_none() {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(SymbolId, String)>()))
            .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len()))
            .saturating_add(usize_to_u64(namespace.map_or(0, str::len))),
    )?;
    if let Some(namespace) = namespace {
        templates
            .namespaces
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        templates
            .namespaces
            .insert(symbol.input.symbol_id.clone(), try_clone_text(namespace)?);
    }
    if let Some(role) = role {
        templates
            .roles
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        templates.roles.insert(symbol.input.symbol_id.clone(), role);
    }
    Ok(())
}

fn declaration_role(body: &str) -> Option<&'static str> {
    let tag = body
        .strip_prefix("mybatis ")?
        .split_ascii_whitespace()
        .next()?;
    ["resultMap", "parameterMap", "sql"]
        .into_iter()
        .find(|role| tag.eq_ignore_ascii_case(role))
}

struct TemplateQuery<'a> {
    role: &'a str,
    name: &'a str,
    namespace: &'a str,
    local: bool,
}

fn query<'a>(
    index: &'a ResolutionIndex,
    request: &'a ResolutionRequest<'_>,
) -> Option<TemplateQuery<'a>> {
    if let Some(qualified) = request.name.strip_prefix(TEMPLATE_PREFIX) {
        let (qualified, role) = qualified.rsplit_once("::")?;
        let (namespace, name) = qualified.rsplit_once("::")?;
        return Some(TemplateQuery {
            role,
            name,
            namespace,
            local: false,
        });
    }
    let (name, role) = request.name.strip_prefix(LOCAL_PREFIX)?.rsplit_once("::")?;
    let namespace = request
        .owner
        .and_then(|owner| index.parents.get(owner))
        .and_then(|parent| index.route_bridges.templates.namespaces.get(parent))?;
    Some(TemplateQuery {
        role,
        name,
        namespace,
        local: true,
    })
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "xml" || request.kind != ReferenceKind::References {
        return Ok(None);
    }
    if matches!(
        request.name,
        UNPROVEN_CLASS_LOOKUP | UNPROVEN_TEMPLATE_LOOKUP
    ) {
        return Ok(Some(ReferenceResolution::unresolved(
            UNSUPPORTED_PROVENANCE,
        )));
    }
    if request.owner.is_none() {
        return Ok(None);
    }
    let Some(query) = query(index, request) else {
        return Ok(None);
    };
    let Some(candidates) = index.candidates.get(query.name) else {
        return Ok(Some(ReferenceResolution::unresolved(
            TEMPLATE_UNRESOLVED_PROVENANCE,
        )));
    };
    let candidate = select_candidate(
        candidates.iter(),
        |candidate| template_matches(index, candidate, &query),
        cancelled,
    )?;
    Ok(Some(candidate.map_or_else(
        || ReferenceResolution::unresolved(TEMPLATE_UNRESOLVED_PROVENANCE),
        |candidate| {
            let local = query.local && candidate.file_id == *request.file_id;
            ReferenceResolution::resolved(ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence: if local {
                    EXACT_SAME_FILE_CONFIDENCE
                } else {
                    TEMPLATE_CONFIDENCE
                },
                provenance: if local {
                    EXACT_SAME_FILE_PROVENANCE
                } else {
                    "framework-mybatis-template-qualified-namespace"
                },
            })
        },
    )))
}

fn template_matches(
    index: &ResolutionIndex,
    candidate: &ResolutionCandidate,
    query: &TemplateQuery<'_>,
) -> bool {
    let templates = &index.route_bridges.templates;
    if templates.roles.get(&candidate.symbol_id).copied() != Some(query.role) {
        return false;
    }
    candidate
        .parent_symbol_id
        .as_ref()
        .and_then(|parent| templates.namespaces.get(parent))
        .is_some_and(|written| written == query.namespace)
}
