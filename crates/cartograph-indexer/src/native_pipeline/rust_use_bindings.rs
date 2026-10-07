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
    lifetime_functions: Vec<(u64, u64)>,
    macros: Vec<(u64, u64)>,
    /// Item-position invocations can introduce items throughout a module scope.
    item_macros: Vec<(u64, u64)>,
    /// Unknown statement macros can introduce items throughout their block.
    block_macros: Vec<(u64, u64)>,
    standard_blocks: Vec<(u64, u64)>,
    standard_items: Vec<(u64, u64)>,
    /// Standard spellings whose only uncertainty is a declared `super::*`.
    super_macros: LocalSpans,
    defined_macros: LocalSpans,
    locals: LocalSpans,
    root_macro: bool,
    macro_import: bool,
}

type LocalSpans = HashMap<String, Vec<(u64, u64)>>;

#[derive(Default)]
struct MacroImports<'file> {
    overrides: HashSet<&'file str>,
    unknown: Vec<(u64, u64)>,
    super_globs: bool,
}

pub(super) fn metadata_binding(language: &str, binding: &ExtractedImportBinding) -> bool {
    language == "rust"
        && matches!(
            binding.module_specifier.as_str(),
            "<rust-plain-root-impl>"
                | "<rust-lifetime-root-function>"
                | "<rust-inline-file-module>"
                | "<rust-scoped-module-glob>"
                | "<rust-standard-macro-definition>"
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
    let imports = macro_imports(target, file, cancelled)?;
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !metadata_binding("rust", binding) {
            continue;
        }
        retain_binding(target.budget, (&mut scopes, binding), &imports)?;
    }
    if scopes_empty(&scopes) {
        return Ok(());
    }
    scopes.impls.sort_unstable();
    scopes.lifetime_functions.sort_unstable();
    scopes.macros.sort_unstable();
    scopes.item_macros.sort_unstable();
    merge_block_scopes(&mut scopes.block_macros, cancelled)?;
    merge_block_scopes(&mut scopes.standard_blocks, cancelled)?;
    scopes.standard_items.sort_unstable();
    for spans in scopes.super_macros.values_mut() {
        merge_block_scopes(spans, cancelled)?;
    }
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
    let files = &mut target.index.languages.rust_paths.use_scopes.files;
    files.try_reserve(1).map_err(|_| StageItemFailure)?;
    files.insert(file.file.file_id.clone(), scopes);
    Ok(())
}

fn scopes_empty(scopes: &FileScopes) -> bool {
    [
        scopes.impls.is_empty(),
        scopes.lifetime_functions.is_empty(),
        scopes.macros.is_empty(),
        scopes.block_macros.is_empty(),
        scopes.standard_blocks.is_empty(),
        scopes.standard_items.is_empty(),
        scopes.super_macros.is_empty(),
        scopes.defined_macros.is_empty(),
        scopes.locals.is_empty(),
        !scopes.macro_import,
    ]
    .into_iter()
    .all(|empty| empty)
}

fn macro_imports<'file, Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &'file NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<MacroImports<'file>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut imports = MacroImports {
        super_globs: only_super_globs(target.index, file, cancelled)?,
        ..MacroImports::default()
    };
    cargo_macro_overrides(target, (file, &mut imports), cancelled)?;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(name) = super::rust_path_guards::declaration_name(symbol)
            && matches!(name, "std" | "core")
        {
            retain_override(target.budget, (&mut imports.overrides, name))?;
        }
    }
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.module_specifier == "<rust-opaque-macro-import>" {
            push_scope(target.budget, (&mut imports.unknown, binding))?;
        } else if !metadata_binding("rust", binding)
            && matches!(
                binding.kind,
                ImportBindingKind::Namespace | ImportBindingKind::ReExportNamed
            )
            && !standard_import(binding)
        {
            retain_override(
                target.budget,
                (
                    &mut imports.overrides,
                    super::rust_path_guards::raw_name(&binding.local_name),
                ),
            )?;
        }
    }
    withdraw_standard_imports(target.budget, (file, &mut imports), cancelled)?;
    merge_block_scopes(&mut imports.unknown, cancelled)?;
    Ok(imports)
}

fn cargo_macro_overrides<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    query: (&NativeFileFacts, &mut MacroImports<'_>),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, imports) = query;
    for name in ["std", "core"] {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if super::rust_dependency_paths::namespace_override(
            target.index,
            (&file.file.normalized_path, name),
        ) {
            retain_override(target.budget, (&mut imports.overrides, name))?;
        }
    }
    Ok(())
}

fn only_super_globs<Cancel>(
    index: &ResolutionIndex,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut found = false;
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if metadata_binding("rust", binding) || binding.local_name != "*" {
            continue;
        }
        if binding.module_specifier != "super::*"
            || !super::rust_local_types::root_declaration(index, (&file.file.file_id, binding.span))
        {
            return Ok(false);
        }
        found = true;
    }
    Ok(found)
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
        "<rust-lifetime-root-function>" => {
            push_scope(budget, (&mut scopes.lifetime_functions, binding))
        }
        "<rust-opaque-root-macro>" => {
            if matches!(binding.local_name.as_str(), "std" | "core") {
                push_scope(budget, (&mut scopes.standard_items, binding))?;
            }
            if uncertain_macro(binding, imports) {
                scopes.root_macro = true;
                push_scope(budget, (&mut scopes.item_macros, binding))?;
            }
            push_scope(budget, (&mut scopes.macros, binding))
        }
        "<rust-opaque-macro>" => push_scope(budget, (&mut scopes.macros, binding)),
        "<rust-opaque-block-macro>" => retain_macro_block(budget, (scopes, binding), imports),
        "<rust-standard-macro-definition>" => {
            retain_local(budget, (&mut scopes.defined_macros, binding))
        }
        "<rust-opaque-macro-import>" => {
            scopes.macro_import = true;
            Ok(())
        }
        "<rust-inline-file-module>" | "<rust-scoped-module-glob>" => Ok(()),
        _ => retain_local(budget, (&mut scopes.locals, binding)),
    }
}

fn retain_macro_block(
    budget: &mut super::ResolveBudget,
    query: (&mut FileScopes, &ExtractedImportBinding),
    imports: &MacroImports<'_>,
) -> Result<(), StageItemFailure> {
    if matches!(query.1.local_name.as_str(), "std" | "core") {
        push_scope(budget, (&mut query.0.standard_blocks, query.1))?;
    }
    if uncertain_macro(query.1, imports) {
        return retain_block_macro(budget, query, imports);
    }
    Ok(())
}

fn retain_block_macro(
    budget: &mut super::ResolveBudget,
    query: (&mut FileScopes, &ExtractedImportBinding),
    imports: &MacroImports<'_>,
) -> Result<(), StageItemFailure> {
    let (scopes, binding) = query;
    let name = binding.local_name.as_str();
    // Extraction emits at most 23 standard spellings. Keep even malformed
    // metadata bounded, so reference checks cannot grow with file size.
    let supported = imports.super_globs && name != "*";
    let unshadowed = !imports.overrides.contains(name) && imports.unknown.is_empty();
    let within_bound = scopes.super_macros.contains_key(name) || scopes.super_macros.len() < 32;
    if supported && unshadowed && within_bound {
        return retain_local(budget, (&mut scopes.super_macros, binding));
    }
    push_scope(budget, (&mut scopes.block_macros, binding))
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
    let Some(file) = file_scopes(index, request.file_id) else {
        return false;
    };
    contains_span(
        &file.impls,
        (request.span.start_byte(), request.span.end_byte()),
    )
}

pub(super) fn lifetime_function(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &super::SymbolId),
) -> bool {
    let (request, owner) = query;
    let Some(owner) = index.qualtype.owners.get(owner) else {
        return false;
    };
    let Some((file, start, end)) = owner.source_scope.as_ref() else {
        return false;
    };
    if owner.kind != SymbolKind::Function || file != request.file_id {
        return false;
    }
    let Some(scopes) = file_scopes(index, file) else {
        return false;
    };
    let position = scopes
        .lifetime_functions
        .partition_point(|(position, _)| position < start);
    scopes.lifetime_functions.get(position) == Some(&(*start, *end))
}

pub(super) fn opaque_macro(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    let Some(file) = file_scopes(index, request.file_id) else {
        return false;
    };
    contains_span(
        &file.macros,
        (request.span.start_byte(), request.span.end_byte()),
    )
}

pub(super) fn root_macro(index: &ResolutionIndex, file: &FileId) -> bool {
    file_scopes(index, file).is_some_and(|scopes| {
        scopes.root_macro
            || !scopes.standard_items.is_empty()
                && super::rust_path_guards::namespace_overridden(index, file)
    })
}

pub(super) fn macro_scope_uncertain(index: &ResolutionIndex, query: (&FileId, &str)) -> bool {
    let (file, name) = query;
    file_scopes(index, file).is_some_and(|scope| {
        root_macro(index, file) || scope.macro_import || scope.defined_macros.contains_key(name)
    })
}

fn file_scopes<'index>(
    index: &'index ResolutionIndex,
    file: &FileId,
) -> Option<&'index FileScopes> {
    index.languages.rust_paths.use_scopes.files.get(file)
}

fn contains_span(scopes: &[(u64, u64)], span: (u64, u64)) -> bool {
    let end = scopes.partition_point(|(start, _)| *start <= span.0);
    end.checked_sub(1)
        .and_then(|position| scopes.get(position))
        .is_some_and(|(_, end)| *end >= span.1)
}

pub(super) fn uncertain_scope<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, (u64, u64)),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, scope) = query;
    let Some(file) = file_scopes(index, request.file_id) else {
        return Ok(false);
    };
    if standard_namespace_uncertain(index, (request, scope, file)) {
        return Ok(true);
    }
    // Module items affect their whole scope. Unknown block statements affect
    // only references in that block, including those before the invocation.
    if starts_in_scope(&file.item_macros, scope)
        || contains_span(
            &file.block_macros,
            (request.span.start_byte(), request.span.end_byte()),
        )
    {
        return Ok(true);
    }
    for (name, spans) in &file.super_macros {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if contains_span(spans, (request.span.start_byte(), request.span.end_byte()))
            && !super::rust_scoped_modules::standard_macro(
                index,
                (request.file_id, name),
                cancelled,
            )?
        {
            return Ok(true);
        }
    }
    // Patterns bind values; they do not shadow names in Rust's type namespace,
    // and a path head (`module::item`) resolves in that namespace too.
    if super::qualtype_resolution::nominal(request.kind) || request.name.contains("::") {
        return Ok(false);
    }
    let name = request.name.strip_prefix("r#").unwrap_or(request.name);
    if !name.is_ascii() {
        // Without NFC identity, an ASCII local (for example K) can also
        // shadow a differently spelled Unicode reference (Kelvin sign).
        return Ok(true);
    }
    Ok([name, "*"].into_iter().any(|name| {
        file.locals
            .get(name)
            .is_some_and(|spans| starts_in_scope(spans, scope))
    }))
}

fn standard_namespace_uncertain(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, (u64, u64), &FileScopes),
) -> bool {
    let (request, scope, file) = query;
    super::rust_path_guards::namespace_overridden(index, request.file_id)
        && (starts_in_scope(&file.standard_items, scope)
            || contains_span(
                &file.standard_blocks,
                (request.span.start_byte(), request.span.end_byte()),
            ))
}

pub(super) fn standard_namespace_fenced(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
) -> bool {
    if !super::rust_path_guards::namespace_overridden(index, request.file_id) {
        return false;
    }
    file_scopes(index, request.file_id).is_some_and(|file| {
        !file.standard_items.is_empty()
            || contains_span(
                &file.standard_blocks,
                (request.span.start_byte(), request.span.end_byte()),
            )
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
    if request.name.contains("r#") {
        // Public declaration keys retain source spelling. Do not add proof
        // until every nearer item/generic key can establish raw identity.
        return Ok(false);
    }
    if !super::rust_local_types::root_declaration(index, (request.file_id, binding.span))
        || !super::rust_local_types::use_scope_proven(index, request, cancelled)?
    {
        return Ok(false);
    }
    unshadowed(index, (request, None), cancelled)
}

pub(super) fn unshadowed<Cancel>(
    index: &ResolutionIndex,
    query: (
        &ResolutionRequest<'_>,
        Option<super::rust_inline_modules::ModuleScope<'_>>,
    ),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, module) = query;
    let name = request.name.split("::").next().unwrap_or(request.name);
    let candidates = resolution_candidates_for_file(index, name, request.file_id);
    let mut scope = request.owner;
    for _ in 0..=index.parents.len().saturating_add(1) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if module.is_some_and(|module| scope == module.module) {
            return Ok(true);
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
