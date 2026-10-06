//! File-module uses of `self::inline::Type` can name a public type in this file.
//! Nested imports and impl members need richer binding evidence and abstain.

use std::collections::HashSet;

use super::{
    ExtractedImportBinding, FileId, ImportBindingKind, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceResolution, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, StageItemFailure, SymbolKind,
    qualtype_resolution::{Selection, nominal, nominal_candidate},
    reference_kind_candidate, resolution_candidates_for_file, size_of, usize_to_u64,
};

pub(super) const PROVENANCE: &str = "native-rust-inline-type";

#[derive(Default)]
pub(super) struct RootImports {
    files: HashSet<FileId>,
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "rust" {
        return Ok(());
    }
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind == SymbolKind::Import
            && target.index.parents.contains_key(&symbol.input.symbol_id)
        {
            return Ok(());
        }
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<FileId>()))
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
    )?;
    target
        .index
        .qualtype
        .rust
        .files
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .qualtype
        .rust
        .files
        .insert(file.file.file_id.clone());
    Ok(())
}

#[derive(Clone, Copy)]
struct InlineType<'a, 'b> {
    index: &'a ResolutionIndex,
    request: &'a ResolutionRequest<'b>,
    module: &'a str,
    name: &'a str,
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
    let mut selected = Selection::default();
    let mut bound = false;
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some((module, name)) = inline_import(binding, request.name) else {
            continue;
        };
        bound = true;
        if !index.qualtype.rust.files.contains(request.file_id)
            || !file_module_scope(index, request, cancelled)?
            || super::qualtype_generics::blocked(index, request, cancelled)?
        {
            return Ok(None);
        }
        retain(
            &mut selected,
            InlineType {
                index,
                request,
                module,
                name,
            },
            cancelled,
        )?;
    }
    // An absent inline declaration can instead live in a physical submodule file.
    Ok(if bound {
        selected.resolution(PROVENANCE, 1.0)
    } else {
        None
    })
}

fn inline_import<'a>(
    binding: &'a ExtractedImportBinding,
    name: &str,
) -> Option<(&'a str, &'a str)> {
    if binding.kind != ImportBindingKind::Namespace || binding.local_name != name {
        return None;
    }
    binding
        .module_specifier
        .strip_prefix("self::")?
        .rsplit_once("::")
}

fn file_module_scope<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut owner = request.owner;
    for _ in 0..=index.parents.len() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else {
            return Ok(true);
        };
        if index.qualtype.owners.get(id).is_some_and(|owner| {
            matches!(owner.kind, SymbolKind::Module | SymbolKind::Method)
                || (matches!(owner.kind, SymbolKind::TypeAlias | SymbolKind::Constant)
                    && owner.name.contains("::"))
                || owner
                    .source_scope
                    .as_ref()
                    .is_none_or(|(file, start, end)| {
                        file != request.file_id
                            || *start > request.span.start_byte()
                            || *end < request.span.end_byte()
                    })
        }) {
            return Ok(false);
        }
        owner = index.parents.get(id);
    }
    Ok(false)
}

fn retain<'a, Cancel>(
    selected: &mut Selection<'a>,
    query: InlineType<'a, '_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for candidate in resolution_candidates_for_file(query.index, query.name, query.request.file_id)
    {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let identity = candidate
            .qualified_name
            .strip_prefix(query.module)
            .and_then(|tail| tail.strip_prefix("::"));
        if identity == Some(query.name)
            && candidate.export.exported
            && nominal_candidate(candidate.kind)
            && reference_kind_candidate(query.request.kind, candidate)
        {
            selected.retain(candidate);
        }
    }
    Ok(())
}
