//! Preserve public inline modules and explicit one-hop named reexports.
//! Reexport destinations still traverse the declared file-module graph.
use super::{
    FileId, HashMap, ImportBindingSelection, RESOLUTION_MAP_NODE_ALLOWANCE,
    RUST_QUALIFIED_PATH_PROVENANCE, RUST_WORKSPACE_CRATE_PROVENANCE, ReferenceDispatch,
    ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest, ResolvedTarget,
    StageItemFailure, SymbolId, SymbolKind, Visibility, reference_kind_candidate,
    resolution_candidates_for_file, select_candidate, size_of, try_clone_text, usize_to_u64,
};

use std::collections::HashSet;

#[derive(Default)]
pub(super) struct FacadeIndex {
    by_file: HashMap<FileId, HashMap<String, Option<usize>>>,
    public_targets: HashSet<SymbolId>,
}

pub(super) fn index<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let facades = &mut target.index.rust_paths.facades;
    for (position, export) in target.index.rust_named_re_exports.iter().enumerate() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !facades.by_file.contains_key(&export.source_file_id) {
            target.budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    + usize_to_u64(
                        size_of::<(FileId, HashMap<String, Option<usize>>)>()
                            + export.source_file_id.as_str().len(),
                    ),
            )?;
            facades
                .by_file
                .try_reserve(1)
                .map_err(|_| StageItemFailure)?;
            facades
                .by_file
                .insert(export.source_file_id.clone(), HashMap::new());
        }
        let names = facades
            .by_file
            .get_mut(&export.source_file_id)
            .ok_or(StageItemFailure)?;
        if let Some(existing) = names.get_mut(&export.public_name) {
            *existing = None;
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(String, Option<usize>)>() + export.public_name.len()),
        )?;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(try_clone_text(&export.public_name)?, Some(position));
    }
    index_public_targets(target, cancelled)?;
    Ok(())
}

fn index_public_targets<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let facades = &mut target.index.rust_paths.facades;
    for bucket in target.index.candidates.values() {
        for candidate in bucket.as_slice() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if !candidate.export.exported
                || candidate.visibility != Some(Visibility::Public)
                || facades.public_targets.contains(&candidate.symbol_id)
            {
                continue;
            }
            target.budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    + usize_to_u64(size_of::<SymbolId>() + candidate.symbol_id.as_str().len()),
            )?;
            facades
                .public_targets
                .try_reserve(1)
                .map_err(|_| StageItemFailure)?;
            facades.public_targets.insert(candidate.symbol_id.clone());
        }
    }
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, path) = query;
    let Some((root, name)) = path.split_once("::") else {
        return Ok(None);
    };
    let file = if root == "crate" {
        super::rust_root_ownership::root(index, request.file_id)
    } else {
        super::rust_dependency_paths::entry(index, (request.file_path, root))
    };
    let Some(file) = file else { return Ok(None) };
    let candidates = resolution_candidates_for_file(index, name, file);
    let inline = select_candidate(
        candidates,
        |candidate| {
            candidate.qualified_name == name
                && candidate.export.exported
                && (candidate.visibility == Some(Visibility::Public)
                    || candidate.kind == SymbolKind::EnumMember)
                && reference_kind_candidate(request.kind, candidate)
        },
        cancelled,
    )?;
    let target = if let Some(candidate) = inline {
        if !public_scope(index, (file, name), cancelled)? {
            return Ok(None);
        }
        Some(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 1.0,
            provenance: RUST_QUALIFIED_PATH_PROVENANCE,
        })
    } else {
        named_reexport(index, (request, file, name), cancelled)?
    };
    Ok(target.map(|mut target| {
        target.provenance = if root == "crate" {
            RUST_QUALIFIED_PATH_PROVENANCE
        } else {
            RUST_WORKSPACE_CRATE_PROVENANCE
        };
        target
    }))
}

fn named_reexport<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &FileId, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, file, name) = query;
    let Some(export) = reexport(index, (file, name), cancelled)? else {
        return Ok(None);
    };
    if !public_scope(index, (file, &export.public_name), cancelled)? {
        return Ok(None);
    }
    let Some(suffix) = name.strip_prefix(&export.public_name) else {
        return Ok(None);
    };
    let prefix =
        if super::rust_path_resolution::rooted_path(&index.modules, &export.module_specifier) {
            ""
        } else {
            "crate::"
        };
    if prefix
        .len()
        .saturating_add(export.module_specifier.len())
        .saturating_add(suffix.len())
        > 1_024
    {
        return Ok(None);
    }
    let path = format!("{prefix}{}{suffix}", export.module_specifier);
    let Some(context) = index.modules.files.get(file) else {
        return Err(StageItemFailure);
    };
    let local = ResolutionRequest {
        file_id: file,
        file_path: &context.path,
        owner: None,
        import_bindings: ImportBindingSelection::empty(),
        dispatch: ReferenceDispatch::Static,
        ..*request
    };
    let target = super::rust_path_resolution::resolve_path(index, (&local, &path), cancelled)?;
    let Some(target) = target else {
        return Ok(None);
    };
    Ok(index
        .rust_paths
        .facades
        .public_targets
        .contains(&target.symbol_id)
        .then_some(target))
}

fn reexport<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (&FileId, &str),
    cancelled: &mut Cancel,
) -> Result<Option<&'a super::RustNamedReExport>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, mut name) = query;
    let Some(names) = index.rust_paths.facades.by_file.get(file) else {
        return Ok(None);
    };
    let mut retained = None;
    loop {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(position) = names.get(name) {
            if retained.is_some() || position.is_none() {
                return Ok(None);
            }
            retained = *position;
        }
        let Some((parent, _)) = name.rsplit_once("::") else {
            break;
        };
        name = parent;
    }
    Ok(retained.and_then(|position| index.rust_named_re_exports.get(position)))
}

fn public_scope<Cancel>(
    index: &ResolutionIndex,
    query: (&FileId, &str),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, mut name) = query;
    while let Some((parent, _)) = name.rsplit_once("::") {
        let candidates = resolution_candidates_for_file(index, parent, file);
        let candidate = select_candidate(
            candidates,
            |candidate: &ResolutionCandidate| {
                candidate.qualified_name == parent
                    && candidate.export.exported
                    && candidate.visibility == Some(Visibility::Public)
                    && matches!(
                        candidate.kind,
                        SymbolKind::Module
                            | SymbolKind::Struct
                            | SymbolKind::Enum
                            | SymbolKind::Trait
                    )
            },
            cancelled,
        )?;
        if candidate.is_none() {
            return Ok(false);
        }
        name = parent;
    }
    Ok(true)
}
