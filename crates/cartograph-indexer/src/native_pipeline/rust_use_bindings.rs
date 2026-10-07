//! Rust use paths cannot bypass the verified scope and shadow evidence used by
//! nominal lookup. Only verified top-level impls without generics add method
//! scope proof; unsupported scopes abstain to the base tiers.
use std::collections::HashSet;

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
    /// Item-position invocations can introduce items throughout a module scope.
    item_macros: Vec<(u64, u64)>,
    /// Unknown statement macros can introduce items throughout their block.
    block_macros: Vec<(u64, u64)>,
    locals: LocalSpans,
    root_macro: bool,
}

type LocalSpans = HashMap<String, Vec<(u64, u64)>>;

#[derive(Default)]
struct MacroImports<'file> {
    overrides: HashSet<&'file str>,
    unknown: Vec<(u64, u64)>,
}

pub(super) fn metadata_binding(language: &str, binding: &ExtractedImportBinding) -> bool {
    language == "rust"
        && matches!(
            binding.module_specifier.as_str(),
            "<rust-plain-root-impl>"
                | "<rust-unrepresented-locals>"
                | "<rust-opaque-macro>"
                | "<rust-opaque-root-macro>"
                | "<rust-opaque-block-macro>"
                | "<rust-opaque-macro-import>"
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
    let imports = macro_imports(target.budget, file, cancelled)?;
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !metadata_binding("rust", binding) {
            continue;
        }
        retain_binding(target.budget, (&mut scopes, binding), &imports)?;
    }
    if scopes.impls.is_empty()
        && scopes.macros.is_empty()
        && scopes.block_macros.is_empty()
        && scopes.locals.is_empty()
    {
        return Ok(());
    }
    scopes.impls.sort_unstable();
    scopes.macros.sort_unstable();
    scopes.item_macros.sort_unstable();
    merge_block_scopes(&mut scopes.block_macros, cancelled)?;
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

fn macro_imports<'file, Cancel>(
    budget: &mut super::ResolveBudget,
    file: &'file NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<MacroImports<'file>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut imports = MacroImports::default();
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind == SymbolKind::Module && matches!(symbol.name.as_str(), "std" | "core") {
            retain_override(budget, (&mut imports.overrides, &symbol.name))?;
        }
    }
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.module_specifier == "<rust-opaque-macro-import>" {
            push_scope(budget, (&mut imports.unknown, binding))?;
        } else if !metadata_binding("rust", binding)
            && matches!(
                binding.kind,
                ImportBindingKind::Namespace | ImportBindingKind::ReExportNamed
            )
            && !standard_import(binding)
        {
            retain_override(budget, (&mut imports.overrides, &binding.local_name))?;
        }
    }
    withdraw_standard_imports(budget, (file, &mut imports), cancelled)?;
    merge_block_scopes(&mut imports.unknown, cancelled)?;
    Ok(imports)
}

fn retain_override<'file>(
    budget: &mut super::ResolveBudget,
    entry: (&mut HashSet<&'file str>, &'file str),
) -> Result<(), StageItemFailure> {
    let (overrides, name) = entry;
    if overrides.contains(name) {
        return Ok(());
    }
    budget.charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<&str>()))?;
    overrides.try_reserve(1).map_err(|_| StageItemFailure)?;
    overrides.insert(name);
    Ok(())
}

fn withdraw_standard_imports<'file, Cancel>(
    budget: &mut super::ResolveBudget,
    entry: (&'file NativeFileFacts, &mut MacroImports<'file>),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, imports) = entry;
    if !imports.overrides.contains("std") && !imports.overrides.contains("core") {
        return Ok(());
    }
    // Chained imports through a shadowed standard root have no proven
    // standard identity. Withhold this bounded subset conservatively.
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if matches!(
            binding.kind,
            ImportBindingKind::Namespace | ImportBindingKind::ReExportNamed
        ) && standard_import(binding)
        {
            retain_override(budget, (&mut imports.overrides, &binding.local_name))?;
        }
    }
    Ok(())
}

fn standard_import(binding: &ExtractedImportBinding) -> bool {
    let module = binding.module_specifier.as_str();
    let module = module.strip_prefix("::").unwrap_or(module);
    if matches!(binding.local_name.as_str(), "std" | "core") && matches!(module, "std" | "core") {
        return true;
    }
    module
        .strip_prefix("std::")
        .or_else(|| module.strip_prefix("core::"))
        == Some(binding.local_name.as_str())
}

fn uncertain_macro(binding: &ExtractedImportBinding, imports: &MacroImports<'_>) -> bool {
    let name = binding.local_name.as_str();
    name == "*"
        || imports.overrides.contains(name)
        || imports.overrides.contains("*")
        || (!matches!(name, "std" | "core")
            && binding
                .imported_name
                .parse::<u64>()
                .ok()
                .is_none_or(|position| contains_span(&imports.unknown, (position, position))))
}

fn retain_binding(
    budget: &mut super::ResolveBudget,
    entry: (&mut FileScopes, &ExtractedImportBinding),
    imports: &MacroImports<'_>,
) -> Result<(), StageItemFailure> {
    let (scopes, binding) = entry;
    match binding.module_specifier.as_str() {
        "<rust-plain-root-impl>" => push_scope(budget, (&mut scopes.impls, binding)),
        "<rust-opaque-root-macro>" => {
            scopes.root_macro = true;
            push_scope(budget, (&mut scopes.item_macros, binding))?;
            push_scope(budget, (&mut scopes.macros, binding))
        }
        "<rust-opaque-macro>" => push_scope(budget, (&mut scopes.macros, binding)),
        "<rust-opaque-block-macro>" if uncertain_macro(binding, imports) => {
            push_scope(budget, (&mut scopes.block_macros, binding))
        }
        "<rust-opaque-block-macro>" | "<rust-opaque-macro-import>" => Ok(()),
        _ => retain_local(budget, (&mut scopes.locals, binding)),
    }
}

fn merge_block_scopes<Cancel>(
    scopes: &mut Vec<(u64, u64)>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    scopes.sort_unstable();
    let mut retained = 0;
    for position in 0..scopes.len() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let (start, end) = scopes[position];
        if retained > 0 && start <= scopes[retained - 1].1 {
            scopes[retained - 1].1 = scopes[retained - 1].1.max(end);
        } else {
            scopes[retained] = (start, end);
            retained += 1;
        }
    }
    scopes.truncate(retained);
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
    // Module items affect their whole scope. Unknown block statements affect
    // only references in that block, including those before the invocation.
    if starts_in_scope(&file.item_macros, scope)
        || contains_span(
            &file.block_macros,
            (request.span.start_byte(), request.span.end_byte()),
        )
    {
        return true;
    }
    // Patterns bind values; they do not shadow names in Rust's type namespace,
    // and a path head (`module::item`) resolves in that namespace too.
    if super::qualtype_resolution::nominal(request.kind) || request.name.contains("::") {
        return false;
    }
    let name = request.name;
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
