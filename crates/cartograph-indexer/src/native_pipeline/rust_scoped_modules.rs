//! Declared inline-to-file edges and a bounded two-hop `super::*` path proof.
use super::{
    ExtractedImportBinding, FileId, ImportBindingKind, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest,
    ResolvedTarget, StageItemFailure, SymbolKind, rust_inline_modules,
    rust_path_resolution::ModuleEdge, size_of, usize_to_u64,
};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct ScopedImports {
    files: HashMap<FileId, HashMap<String, Imports>>,
}

#[derive(Default)]
struct Imports {
    super_glob: bool,
    opaque_glob: bool,
    names: HashSet<String>,
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
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.module_specifier == "<rust-inline-file-module>" {
            file_edge(target, (file, binding))?;
        } else if binding.module_specifier == "<rust-scoped-module-glob>"
            || matches!(
                binding.kind,
                ImportBindingKind::Namespace | ImportBindingKind::ReExportNamed
            ) && !super::rust_use_bindings::metadata_binding("rust", binding)
        {
            index_import(target, (file, binding))?;
        }
    }
    Ok(())
}

fn file_edge(
    target: &mut ResolutionIndexTarget<'_>,
    query: (&NativeFileFacts, &ExtractedImportBinding),
) -> Result<(), StageItemFailure> {
    let (file, binding) = query;
    let parent = super::rust_local_types::declaration_scope(
        target.index,
        (&file.file.file_id, binding.span),
    )
    .and_then(|scope| scope.owner);
    let Some(parent_id) = parent else {
        return Ok(());
    };
    let parent = target.index.qualtype.owners.get(parent_id);
    if parent.is_none_or(|parent| {
        parent.kind != SymbolKind::Module
            || parent.name != binding.local_name
            || parent.source_scope.as_ref().is_none_or(|(id, start, end)| {
                id != &file.file.file_id
                    || *start > binding.span.start_byte()
                    || *end < binding.span.end_byte()
            })
    }) {
        return Ok(());
    }
    let parent_id = parent_id.clone();
    let Some(name) =
        rust_inline_modules::qualified_name(&binding.local_name, &binding.imported_name)?
    else {
        return Ok(());
    };
    let Some(directory) = super::rust_current_module_directory(&file.file.normalized_path) else {
        return Ok(());
    };
    let suffix = name.replace("::", "/");
    let path = super::joined_path(&directory, &suffix)?;
    let child =
        super::resolve_normalized_module_file(&target.index.modules, &path, "rust").cloned();
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(String, Option<ModuleEdge>)>()
                    + name.len()
                    + parent_id.as_str().len()
                    + directory.len()
                    + suffix.len()
                    + path.len()
                    + child.as_ref().map_or(0, |id| id.as_str().len()),
            ),
    )?;
    let edges = edge_map(target, &file.file.file_id)?;
    if let Some(existing) = edges.get_mut(&name) {
        *existing = None;
        return Ok(());
    }
    edges.try_reserve(1).map_err(|_| StageItemFailure)?;
    edges.insert(
        name,
        Some(ModuleEdge {
            file: child,
            visibility: super::rust_path_resolution::ModuleVisibility::Private,
            parent_module: Some(parent_id),
        }),
    );
    Ok(())
}

fn edge_map<'a>(
    target: &'a mut ResolutionIndexTarget<'_>,
    file: &FileId,
) -> Result<&'a mut HashMap<String, Option<ModuleEdge>>, StageItemFailure> {
    if !target.index.languages.rust_paths.modules.contains_key(file) {
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(
                    size_of::<(FileId, HashMap<String, Option<ModuleEdge>>)>()
                        + file.as_str().len(),
                ),
        )?;
        target
            .index
            .languages
            .rust_paths
            .modules
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        target
            .index
            .languages
            .rust_paths
            .modules
            .insert(file.clone(), HashMap::new());
    }
    target
        .index
        .languages
        .rust_paths
        .modules
        .get_mut(file)
        .ok_or(StageItemFailure)
}

fn index_import(
    target: &mut ResolutionIndexTarget<'_>,
    query: (&NativeFileFacts, &ExtractedImportBinding),
) -> Result<(), StageItemFailure> {
    let (file, binding) = query;
    let Some(owner) = super::rust_local_types::declaration_scope(
        target.index,
        (&file.file.file_id, binding.span),
    ) else {
        return Ok(());
    };
    let owner = owner.owner;
    if owner.is_some_and(|id| {
        target
            .index
            .qualtype
            .owners
            .get(id)
            .is_none_or(|scope| scope.kind != SymbolKind::Module)
    }) {
        return Ok(());
    }
    let owner = super::try_clone_text(owner.map_or("", |id| id.as_str()))?;
    let imports = import_map(target, (&file.file.file_id, &owner))?;
    if binding.local_name == "*" {
        if binding.module_specifier == "super::*"
            || binding.module_specifier == "<rust-scoped-module-glob>"
                && binding.imported_name == "super::*"
        {
            imports.super_glob = true;
        } else {
            imports.opaque_glob = true;
        }
        return Ok(());
    }
    let name = binding
        .local_name
        .rsplit("::")
        .next()
        .unwrap_or(&binding.local_name);
    if imports.names.contains(name) {
        return Ok(());
    }
    // Charge the retained name before reserving its hash-table slot.
    target
        .budget
        .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<String>() + name.len()))?;
    let imports = import_map(target, (&file.file.file_id, &owner))?;
    imports.names.try_reserve(1).map_err(|_| StageItemFailure)?;
    imports.names.insert(super::try_clone_text(name)?);
    Ok(())
}

fn import_map<'a>(
    target: &'a mut ResolutionIndexTarget<'_>,
    query: (&FileId, &str),
) -> Result<&'a mut Imports, StageItemFailure> {
    let (file, owner) = query;
    let files = &mut target.index.languages.rust_paths.scoped_imports.files;
    if !files.contains_key(file) {
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(
                    size_of::<(FileId, HashMap<String, Imports>)>() + file.as_str().len(),
                ),
        )?;
        files.try_reserve(1).map_err(|_| StageItemFailure)?;
        files.insert(file.clone(), HashMap::new());
    }
    let scopes = files.get_mut(file).ok_or(StageItemFailure)?;
    if !scopes.contains_key(owner) {
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(String, Imports)>() + owner.len()),
        )?;
        scopes.try_reserve(1).map_err(|_| StageItemFailure)?;
        scopes.insert(super::try_clone_text(owner)?, Imports::default());
    }
    scopes.get_mut(owner).ok_or(StageItemFailure)
}

pub(super) fn resolve_glob<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(mut scope) = rust_inline_modules::enclosing(index, request, cancelled)? else {
        return Ok(None);
    };
    let head = request.name.split("::").next().unwrap_or(request.name);
    for _ in 0..2 {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let imports = index
            .languages
            .rust_paths
            .scoped_imports
            .files
            .get(scope.file)
            .and_then(|scopes| scopes.get(scope.module.map_or("", |id| id.as_str())));
        if imports.is_none_or(|imports| {
            !imports.super_glob || imports.opaque_glob || imports.names.contains(head)
        }) || super::rust_use_bindings::root_macro(index, scope.file)
            || super::rust_uniform_paths::declared_head(index, (scope, request.name), cancelled)?
        {
            return Ok(None);
        }
        let Some(parent) = super::rust_root_ownership::parent_module(index, scope) else {
            return Ok(None);
        };
        scope = parent;
        if super::rust_uniform_paths::declared_head(index, (scope, request.name), cancelled)? {
            return super::rust_path_resolution::resolve_in_scope(
                index,
                (request, scope, request.name),
                cancelled,
            );
        }
    }
    Ok(None)
}

/// Macro textual scope follows declared ancestors even without glob imports.
/// An incomplete chain, definition, competing import or unknown macro abstains.
pub(super) fn standard_macro<Cancel>(
    index: &ResolutionIndex,
    query: (&FileId, &str),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, name) = query;
    let Some(root) = super::rust_root_ownership::root(index, file) else {
        return Ok(false);
    };
    let mut scope = rust_inline_modules::ModuleScope {
        file,
        inline: "",
        module: None,
    };
    for _ in 0..32 {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if super::rust_use_bindings::macro_scope_uncertain(index, (scope.file, name))
            || super::rust_uniform_paths::declared_head(index, (scope, name), cancelled)?
        {
            return Ok(false);
        }
        let imports = index
            .languages
            .rust_paths
            .scoped_imports
            .files
            .get(scope.file)
            .and_then(|scopes| scopes.get(scope.module.map_or("", |id| id.as_str())));
        if imports.is_some_and(|imports| imports.opaque_glob || imports.names.contains(name)) {
            return Ok(false);
        }
        if scope.file == root && scope.module.is_none() {
            return Ok(true);
        }
        let Some(parent) = super::rust_root_ownership::parent_module(index, scope) else {
            return Ok(false);
        };
        scope = parent;
    }
    Ok(false)
}
