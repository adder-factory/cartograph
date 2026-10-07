//! Rust use paths cannot bypass the verified scope and shadow evidence used by
//! nominal lookup. Only verified top-level impls without generics add method
//! scope proof; unsupported scopes abstain to the base tiers.
use super::{
    ExtractedImportBinding, FileId, HashMap, ImportBindingKind, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, StageItemFailure, SymbolKind, resolution_candidates_for_file, size_of,
    usize_to_u64,
};

#[derive(Default)]
pub(super) struct ImplScopes {
    files: HashMap<FileId, FileScopes>,
}

#[derive(Default)]
struct FileScopes {
    impls: Vec<(u64, u64)>,
    macros: Vec<(u64, u64)>,
    locals: LocalSpans,
    root_macro: bool,
}

type LocalSpans = HashMap<String, Vec<(u64, u64)>>;

pub(super) fn metadata_binding(language: &str, binding: &ExtractedImportBinding) -> bool {
    language == "rust"
        && matches!(
            binding.module_specifier.as_str(),
            "<rust-plain-root-impl>"
                | "<rust-unrepresented-locals>"
                | "<rust-opaque-macro>"
                | "<rust-opaque-root-macro>"
        )
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut scopes = FileScopes::default();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !metadata_binding("rust", binding) {
            continue;
        }
        if binding.module_specifier == "<rust-plain-root-impl>" {
            push_scope(target.budget, (&mut scopes.impls, binding))?;
        } else if matches!(
            binding.module_specifier.as_str(),
            "<rust-opaque-macro>" | "<rust-opaque-root-macro>"
        ) {
            scopes.root_macro |= binding.module_specifier == "<rust-opaque-root-macro>";
            push_scope(target.budget, (&mut scopes.macros, binding))?;
        } else {
            retain_local(target.budget, (&mut scopes.locals, binding))?;
        }
    }
    if scopes.impls.is_empty() && scopes.macros.is_empty() && scopes.locals.is_empty() {
        return Ok(());
    }
    scopes.impls.sort_unstable();
    scopes.macros.sort_unstable();
    for spans in scopes.locals.values_mut() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        spans.sort_unstable();
    }
    if cancelled() {
        return Err(StageItemFailure);
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(size_of::<(FileId, FileScopes)>() + file.file.file_id.as_str().len()),
    )?;
    let files = &mut target.index.rust_paths.use_scopes.files;
    files.try_reserve(1).map_err(|_| StageItemFailure)?;
    files.insert(file.file.file_id.clone(), scopes);
    Ok(())
}

fn retain_local(
    budget: &mut super::ResolveBudget,
    entry: (&mut LocalSpans, &ExtractedImportBinding),
) -> Result<(), StageItemFailure> {
    let (locals, binding) = entry;
    if !locals.contains_key(&binding.local_name) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(String, Vec<(u64, u64)>)>() + binding.local_name.len()),
        )?;
        locals.try_reserve(1).map_err(|_| StageItemFailure)?;
        locals.insert(super::try_clone_text(&binding.local_name)?, Vec::new());
    }
    let spans = locals
        .get_mut(&binding.local_name)
        .ok_or(StageItemFailure)?;
    push_scope(budget, (spans, binding))
}

fn push_scope(
    budget: &mut super::ResolveBudget,
    entry: (&mut Vec<(u64, u64)>, &ExtractedImportBinding),
) -> Result<(), StageItemFailure> {
    let (scopes, binding) = entry;
    let previous_capacity = scopes.capacity();
    scopes.try_reserve(1).map_err(|_| StageItemFailure)?;
    budget.charge(
        usize_to_u64(scopes.capacity().saturating_sub(previous_capacity))
            .saturating_mul(usize_to_u64(size_of::<(u64, u64)>())),
    )?;
    scopes.push((binding.span.start_byte(), binding.span.end_byte()));
    Ok(())
}

pub(super) fn plain_impl(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    let Some(file) = index.rust_paths.use_scopes.files.get(request.file_id) else {
        return false;
    };
    contains_span(
        &file.impls,
        (request.span.start_byte(), request.span.end_byte()),
    )
}

pub(super) fn opaque_macro(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    let Some(file) = index.rust_paths.use_scopes.files.get(request.file_id) else {
        return false;
    };
    contains_span(
        &file.macros,
        (request.span.start_byte(), request.span.end_byte()),
    )
}

pub(super) fn root_macro(index: &ResolutionIndex, file: &FileId) -> bool {
    index
        .rust_paths
        .use_scopes
        .files
        .get(file)
        .is_some_and(|scopes| scopes.root_macro)
}

fn contains_span(scopes: &[(u64, u64)], span: (u64, u64)) -> bool {
    let end = scopes.partition_point(|(start, _)| *start <= span.0);
    end.checked_sub(1)
        .and_then(|position| scopes.get(position))
        .is_some_and(|(_, end)| *end >= span.1)
}

pub(super) fn uncertain_scope(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, (u64, u64)),
) -> bool {
    let (request, scope) = query;
    let Some(file) = index.rust_paths.use_scopes.files.get(request.file_id) else {
        return false;
    };
    // Opaque macro expansions can introduce caller-visible items and locals.
    if starts_in_scope(&file.macros, scope) {
        return true;
    }
    // Patterns bind values; they do not shadow names in Rust's type namespace.
    if super::qualtype_resolution::nominal(request.kind) {
        return false;
    }
    let name = request.name.split("::").next().unwrap_or(request.name);
    [name, "*"].into_iter().any(|name| {
        file.locals
            .get(name)
            .is_some_and(|spans| starts_in_scope(spans, scope))
    })
}

fn starts_in_scope(spans: &[(u64, u64)], scope: (u64, u64)) -> bool {
    let position = spans.partition_point(|(start, _)| *start < scope.0);
    spans
        .get(position)
        .is_some_and(|(start, _)| *start < scope.1)
}

pub(super) fn allows<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &ExtractedImportBinding),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, binding) = query;
    if request.language != "rust"
        || binding.kind != ImportBindingKind::Namespace
        || binding.module_specifier.starts_with("./")
    {
        return Ok(true);
    }
    if !super::rust_local_types::use_scope_proven(index, request, cancelled)? {
        return Ok(false);
    }
    let name = request.name.split("::").next().unwrap_or(request.name);
    let candidates = resolution_candidates_for_file(index, name, request.file_id);
    let mut scope = request.owner;
    for _ in 0..=index.parents.len().saturating_add(1) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if candidate.parent_symbol_id.as_ref() == scope {
                return Ok(false);
            }
        }
        let Some(owner) = scope else {
            return Ok(true);
        };
        scope = index.parents.get(owner);
    }
    Ok(false)
}

/// File modules can import private root items, but cannot reach a private
/// declaration inside a child inline module merely because it shares a file.
pub(super) fn visible(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    request.language != "rust"
        || candidate.export.exported
        || !candidate.parent_symbol_id.as_ref().is_some_and(|parent| {
            index
                .qualtype
                .owners
                .get(parent)
                .is_some_and(|owner| owner.kind == SymbolKind::Module)
        })
}
