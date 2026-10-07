//! File-module uses of `self::inline::Type` can name a public type in this file.
//! Each binding needs its own root declaration proof; nested imports abstain.

use std::collections::HashMap;

use super::{
    ExtractedImportBinding, FileId, ImportBindingKind, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceResolution, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, SourceSpan, StageItemFailure, SymbolKind,
    qualtype_resolution::{Selection, nominal, nominal_candidate},
    reference_kind_candidate, resolution_candidates_for_file, size_of, usize_to_u64,
};

pub(super) const PROVENANCE: &str = "native-rust-inline-type";

type ImportScopeSpan = (u64, u64, Option<super::SymbolId>);

pub(super) struct DeclarationScope<'a> {
    pub(super) owner: Option<&'a super::SymbolId>,
}

#[derive(Default)]
pub(super) struct RootImports {
    files: HashMap<FileId, Vec<ImportScopeSpan>>,
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
    target.budget.charge(
        usize_to_u64(file.symbols.len()).saturating_mul(usize_to_u64(size_of::<ImportScopeSpan>())),
    )?;
    let mut declarations = Vec::new();
    declarations
        .try_reserve_exact(file.symbols.len())
        .map_err(|_| StageItemFailure)?;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind == SymbolKind::Import {
            let parent = target.index.parents.get(&symbol.input.symbol_id);
            target
                .budget
                .charge(parent.map_or(0, |id| usize_to_u64(id.as_str().len())))?;
            declarations.push((
                symbol.input.start_byte,
                symbol.input.end_byte,
                parent.cloned(),
            ));
        }
    }
    declarations.sort_unstable();
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(FileId, Vec<ImportScopeSpan>)>()))
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
        .insert(file.file.file_id.clone(), declarations);
    Ok(())
}

pub(super) fn root_declaration(index: &ResolutionIndex, query: (&FileId, SourceSpan)) -> bool {
    declaration_scope(index, query).is_some_and(|scope| scope.owner.is_none())
}

pub(super) fn declaration_scope<'a>(
    index: &'a ResolutionIndex,
    query: (&FileId, SourceSpan),
) -> Option<DeclarationScope<'a>> {
    let (file, span) = query;
    let declarations = index.qualtype.rust.files.get(file)?;
    let position = declarations.partition_point(|(start, _, _)| *start <= span.start_byte());
    let declaration = declarations.get(position.checked_sub(1)?)?;
    (declaration.1 >= span.end_byte()).then_some(DeclarationScope {
        owner: declaration.2.as_ref(),
    })
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
        if !root_declaration(index, (request.file_id, binding.span)) {
            continue;
        }
        bound = true;
        if !import_scope_proven(index, request, cancelled)? {
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

/// Keep inline nominal lookup within its existing file-module subset.
pub(super) fn import_scope_proven<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    scope_proven(index, (request, false), cancelled)
}

/// Use lookups can also consume the exact AST proof of a plain root impl.
pub(super) fn use_scope_proven<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    scope_proven(index, (request, true), cancelled)
}

fn scope_proven<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, bool),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, _) = query;
    Ok(index.qualtype.rust.files.contains_key(request.file_id)
        && !super::rust_use_bindings::root_macro(index, request.file_id)
        && !super::rust_use_bindings::opaque_macro(index, request)
        && file_module_scope(index, query, cancelled)?
        && !super::qualtype_generics::blocks_name(
            index,
            (
                request,
                request.name.split("::").next().unwrap_or(request.name),
            ),
            cancelled,
        )?)
}

fn file_module_scope<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, bool),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, allow_impl) = query;
    let mut owner = request.owner;
    for _ in 0..=index.parents.len().saturating_add(1) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else {
            return Ok(true);
        };
        let Some(evidence) = index.qualtype.owners.get(id) else {
            return Ok(false);
        };
        if invalid_source_scope(evidence, request) {
            return Ok(false);
        }
        if let Some((_, start, end)) = evidence.source_scope.as_ref()
            && super::rust_use_bindings::uncertain_scope(
                index,
                (request, (*start, *end)),
                cancelled,
            )?
        {
            return Ok(false);
        }
        if evidence.kind == SymbolKind::Method {
            return Ok(allow_impl && super::rust_use_bindings::plain_impl(index, request));
        }
        if evidence.kind == SymbolKind::Module
            || (matches!(evidence.kind, SymbolKind::TypeAlias | SymbolKind::Constant)
                && evidence.name.contains("::"))
        {
            return Ok(false);
        }
        // The declaration owner may be a type in another file. Only a syntax
        // proof of the actual impl can stop before that unrelated source scope.
        owner = index.parents.get(id);
    }
    Ok(false)
}

fn invalid_source_scope(
    owner: &super::qualtype_resolution::Owner,
    request: &ResolutionRequest<'_>,
) -> bool {
    owner
        .source_scope
        .as_ref()
        .is_none_or(|(file, start, end)| {
            file != request.file_id
                || *start > request.span.start_byte()
                || *end < request.span.end_byte()
        })
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
