//! Qualified `ReScript` types retain the physical implementation/interface pair.

use super::{
    FileId, HashMap, LexicalScopeQuery, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceKind, ReferenceResolution, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest,
    StageItemFailure,
    qualtype_resolution::{Selection, nominal, nominal_candidate},
    reference_kind_candidate, resolution_candidates_for_file, resolve_lexical_scope, size_of,
    try_clone_text, usize_to_u64,
};

pub(super) const PROVENANCE: &str = "native-rescript-module-type";

#[derive(Default)]
pub(super) struct Modules {
    by_name: HashMap<String, Pair>,
}

struct Pair {
    stem: String,
    implementation: Option<FileId>,
    interface: Option<FileId>,
    ambiguous: bool,
}

pub(super) fn index_file(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
) -> Result<(), StageItemFailure> {
    if file.file.language != "rescript" {
        return Ok(());
    }
    let Some((stem, extension)) = file.file.normalized_path.rsplit_once('.') else {
        return Ok(());
    };
    let name = stem.rsplit('/').next().ok_or(StageItemFailure)?;
    let modules = &mut target.index.qualtype.rescript.by_name;
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(String, Pair)>()))
            .saturating_add(usize_to_u64(name.len() + stem.len()))
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
    )?;
    modules.try_reserve(1).map_err(|_| StageItemFailure)?;
    if !modules.contains_key(name) {
        modules.insert(
            try_clone_text(name)?,
            Pair {
                stem: try_clone_text(stem)?,
                implementation: None,
                interface: None,
                ambiguous: false,
            },
        );
    }
    let pair = modules.get_mut(name).ok_or(StageItemFailure)?;
    pair.ambiguous |= pair.stem != stem;
    match extension {
        "res" => pair.implementation = Some(file.file.file_id.clone()),
        "resi" => pair.interface = Some(file.file.file_id.clone()),
        _ => pair.ambiguous = true,
    }
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
    if !nominal(request.kind) {
        return Ok(None);
    }
    let Some((module, name)) = request.name.split_once('.') else {
        return Ok(None);
    };
    if module.is_empty() || name.contains('.') {
        return Ok(None);
    }
    if module_shadowed(index, (request, module), cancelled)? {
        return Ok(None);
    }
    let Some(pair) = index
        .qualtype
        .rescript
        .by_name
        .get(module)
        .filter(|pair| !pair.ambiguous)
    else {
        return Ok(None);
    };
    let mut implementations = Selection::default();
    let mut interfaces = Selection::default();
    for (file, selected) in [
        (pair.implementation.as_ref(), &mut implementations),
        (pair.interface.as_ref(), &mut interfaces),
    ] {
        let Some(file) = file else {
            continue;
        };
        *selected = select_exports(index, (request, name, file), cancelled)?;
    }
    Ok(select_pair(pair, implementations, interfaces))
}

fn module_shadowed<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, module) = query;
    let module_request = ResolutionRequest {
        name: module,
        kind: ReferenceKind::References,
        ..*request
    };
    resolve_lexical_scope(
        index,
        LexicalScopeQuery {
            request: &module_request,
            candidates: resolution_candidates_for_file(index, module, request.file_id),
        },
        cancelled,
    )
    .map(|target| target.is_some())
}

fn select_exports<'index, Cancel>(
    index: &'index ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str, &FileId),
    cancelled: &mut Cancel,
) -> Result<Selection<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, name, file) = query;
    let mut selected = Selection::default();
    for candidate in resolution_candidates_for_file(index, name, file) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.qualified_name == name
            && candidate.top_level
            && candidate.export.exported
            && nominal_candidate(candidate.kind)
            && reference_kind_candidate(request.kind, candidate)
        {
            selected.retain(candidate);
        }
    }
    Ok(selected)
}

fn select_pair(
    pair: &Pair,
    implementations: Selection<'_>,
    interfaces: Selection<'_>,
) -> Option<ReferenceResolution> {
    // A .resi file is the public signature, not another module candidate.
    // Implementation-only declarations stay private even when their names
    // happen to be present in the project index.
    if pair.interface.is_some() && (interfaces.candidate.is_none() || interfaces.ambiguous) {
        return None;
    }
    if let (Some(implementation), Some(interface)) =
        (implementations.candidate, interfaces.candidate)
        && implementation.kind != interface.kind
    {
        return None;
    }
    let selection = if implementations.candidate.is_some() {
        implementations
    } else {
        interfaces
    };
    selection.resolution(PROVENANCE, 1.0)
}
