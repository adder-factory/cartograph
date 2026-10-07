//! Exact Rust imports and paths rooted in the importing crate or an indexed
//! workspace crate. Explicit bindings take precedence over wildcard imports.

use super::{
    ExtractedImportBinding, FileId, HashMap, IMPORT_BINDING_PROVENANCE, ImportBindingKind,
    ImportResolution, ModulePathIndex, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    RUST_QUALIFIED_PATH_PROVENANCE, RUST_WORKSPACE_CRATE_PROVENANCE, ReferenceDispatch,
    ReferenceResolution, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest, ResolvedTarget,
    StageItemFailure, SymbolKind, Visibility, normalize_relative_module_path,
    qualtype_resolution::Selection, reference_kind_candidate, resolution_candidates_for_file,
    resolve_normalized_module_file, rust_inline_modules::ModuleScope, rust_parent_module_contains,
    rust_uniform_paths::Anchor, select_candidate, size_of, try_clone_text, usize_to_u64,
};

const MAXIMUM_PATH_BYTES: usize = 1_024;

enum ExplicitPath {
    Absent,
    Unique(String),
    Ambiguous,
}

enum ModuleLookup<'a> {
    Known(ModuleScope<'a>),
    Unproven,
    Inaccessible,
}

#[derive(Default)]
pub(super) struct PathIndex {
    pub(super) dependencies: super::rust_dependency_paths::DependencyIndex,
    pub(super) modules: HashMap<FileId, HashMap<String, Option<ModuleEdge>>>,
    pub(super) roots: super::rust_root_ownership::RootIndex,
    pub(super) facades: super::rust_facade_resolution::FacadeIndex,
    pub(super) use_scopes: super::rust_use_bindings::ImplScopes,
    pub(super) scoped_imports: super::rust_scoped_modules::ScopedImports,
}

pub(super) struct ModuleEdge {
    pub(super) file: Option<FileId>,
    pub(super) public: bool,
    pub(super) parent_module: Option<super::SymbolId>,
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
    super::rust_use_bindings::index_file(target, file, cancelled)?;
    let public = collect_module_visibility(target, file, cancelled)?;
    let mut edges = HashMap::<String, Option<ModuleEdge>>::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind != ImportBindingKind::Namespace
            || !binding.module_specifier.starts_with("./")
            || !super::rust_local_types::root_declaration(
                target.index,
                (&file.file.file_id, binding.span),
            )
        {
            continue;
        }
        if let Some(existing) = edges.get_mut(&binding.local_name) {
            *existing = None;
            continue;
        }
        let path =
            normalize_relative_module_path(&file.file.normalized_path, &binding.module_specifier);
        let module = path
            .as_deref()
            .and_then(|path| resolve_normalized_module_file(&target.index.modules, path, "rust"));
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(String, Option<ModuleEdge>)>()))
                .saturating_add(usize_to_u64(binding.local_name.len()))
                .saturating_add(module.map_or(0, |file| usize_to_u64(file.as_str().len()))),
        )?;
        edges.try_reserve(1).map_err(|_| StageItemFailure)?;
        edges.insert(
            try_clone_text(&binding.local_name)?,
            Some(ModuleEdge {
                file: module.cloned(),
                public: public
                    .get(binding.module_specifier.as_str())
                    .copied()
                    .unwrap_or(false),
                parent_module: None,
            }),
        );
    }
    if !edges.is_empty() {
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(
                    FileId,
                    HashMap<String, Option<ModuleEdge>>,
                )>()))
                .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
        )?;
        target
            .index
            .rust_paths
            .modules
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        target
            .index
            .rust_paths
            .modules
            .insert(file.file.file_id.clone(), edges);
    }
    Ok(())
}

/// Collect declared module visibility before resolving namespace-binding edges.
fn collect_module_visibility<'file, Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &'file NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<HashMap<&'file str, bool>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut public = HashMap::new();
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind == SymbolKind::Import && symbol.name.starts_with("./") {
            target.budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<(&str, bool)>())),
            )?;
            public.try_reserve(1).map_err(|_| StageItemFailure)?;
            public.insert(symbol.name.as_str(), symbol.export.exported);
        }
    }
    Ok(public)
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !supported_request(request) {
        return Ok(None);
    }
    let explicit = match explicit_path(index, request, cancelled)? {
        ExplicitPath::Absent => None,
        ExplicitPath::Unique(path) => Some(path),
        ExplicitPath::Ambiguous => return Ok(None),
    };
    let path = explicit.as_deref().unwrap_or(request.name);
    let anchor = super::rust_uniform_paths::anchor(index, (request, path), cancelled)?;
    let Some(path) = anchor.path(path) else {
        return Ok(None);
    };
    let root = path.split("::").next().unwrap_or(path);
    if !rooted_path(&index.modules, path)
        && super::rust_dependency_paths::entry(index, (request.file_path, root)).is_none()
    {
        return Ok(None);
    }
    let mut resolution = resolve_target(index, (request, path), cancelled)?;
    let imported = explicit.is_some() || module_binding(index, request, cancelled)?;
    if imported
        && local_path(path)
        && let ImportResolution::Resolved(target) = &mut resolution
    {
        target.provenance = IMPORT_BINDING_PROVENANCE;
    }
    Ok(super::import_reference_resolution(resolution))
}

fn supported_request(request: &ResolutionRequest<'_>) -> bool {
    request.language == "rust"
        && request.name.len() <= MAXIMUM_PATH_BYTES
        && !request.name.contains("r#")
        && request.dispatch == ReferenceDispatch::Static
}

fn module_binding<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let name = request.name.split("::").next().unwrap_or(request.name);
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind == ImportBindingKind::Namespace
            && binding.local_name == name
            && binding.module_specifier.starts_with("./")
            && super::rust_local_types::root_declaration(index, (request.file_id, binding.span))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Resolve the exact leaf/alias binding before the older filesystem importer.
pub(super) fn resolve_binding<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &ExtractedImportBinding),
    cancelled: &mut Cancel,
) -> Result<Option<ImportResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, _) = query;
    if request.language != "rust" {
        return Ok(None);
    }
    if query.1.local_name == "*" && query.1.module_specifier == "super::*" {
        return super::rust_scoped_modules::resolve_glob(index, request, cancelled).map(|target| {
            target.map(|mut target| {
                target.provenance = IMPORT_BINDING_PROVENANCE;
                ImportResolution::Resolved(target)
            })
        });
    }
    let Some(path) = binding_path(query)? else {
        return Ok(None);
    };
    let anchor = super::rust_uniform_paths::anchor(index, (request, &path), cancelled)?;
    let Some(resolved_path) = anchor.path(&path) else {
        return Ok(None);
    };
    let mut resolution = resolve_target(index, (request, resolved_path), cancelled)?;
    if local_path(resolved_path)
        && let ImportResolution::Resolved(target) = &mut resolution
    {
        target.provenance = IMPORT_BINDING_PROVENANCE;
    }
    Ok(match resolution {
        ImportResolution::NotBound => {
            matches!(anchor, Anchor::Local(_)).then_some(ImportResolution::Unresolved)
        }
        _ => Some(resolution),
    })
}

fn local_path(path: &str) -> bool {
    matches!(path.split("::").next(), Some("crate" | "self" | "super"))
}

fn binding_path(
    query: (&ResolutionRequest<'_>, &ExtractedImportBinding),
) -> Result<Option<String>, StageItemFailure> {
    let (request, binding) = query;
    if binding.kind != ImportBindingKind::Namespace
        || binding.local_name == "*"
        || binding.module_specifier.starts_with("./")
    {
        return Ok(None);
    }
    let Some(suffix) = request
        .name
        .strip_prefix(&binding.local_name)
        .filter(|suffix| suffix.is_empty() || suffix.starts_with("::"))
    else {
        return Ok(None);
    };
    let length = binding.module_specifier.len().saturating_add(suffix.len());
    if length > MAXIMUM_PATH_BYTES {
        return Ok(None);
    }
    let mut path = String::new();
    path.try_reserve_exact(length)
        .map_err(|_| StageItemFailure)?;
    path.push_str(&binding.module_specifier);
    path.push_str(suffix);
    Ok(Some(path))
}

fn explicit_path<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ExplicitPath, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut retained = None;
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !super::rust_local_types::root_declaration(index, (request.file_id, binding.span)) {
            continue;
        }
        let Some(path) = binding_path((request, binding))? else {
            continue;
        };
        if !super::rust_use_bindings::allows(index, (request, binding), cancelled)? {
            return Ok(ExplicitPath::Absent);
        }
        if retained.is_some() {
            return Ok(ExplicitPath::Ambiguous);
        }
        retained = Some(path);
    }
    Ok(retained.map_or(ExplicitPath::Absent, ExplicitPath::Unique))
}

pub(super) fn rooted_path(modules: &ModulePathIndex, path: &str) -> bool {
    path.split_once("::").is_some_and(|(root, _)| {
        matches!(root, "crate" | "self" | "super") || modules.rust_packages.contains_key(root)
    })
}

pub(super) fn resolve_path<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    Ok(match checked_path(index, query, cancelled)? {
        ImportResolution::Resolved(target) => Some(target),
        _ => None,
    })
}

fn resolve_target<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let resolution = checked_path(index, query, cancelled)?;
    if !matches!(resolution, ImportResolution::NotBound) {
        return Ok(resolution);
    }
    Ok(
        super::rust_facade_resolution::resolve(index, query, cancelled)?
            .map_or(ImportResolution::NotBound, ImportResolution::Resolved),
    )
}

fn checked_path<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, path) = query;
    let Some((module, name)) = path.rsplit_once("::") else {
        return Ok(ImportResolution::NotBound);
    };
    let resolution = resolve_member(index, (request, module, name), cancelled)?;
    if !matches!(resolution, ImportResolution::NotBound) {
        return Ok(resolution);
    }
    let Some((parent_module, _)) = module.rsplit_once("::") else {
        return Ok(ImportResolution::NotBound);
    };
    let Some(associated) = path
        .strip_prefix(parent_module)
        .and_then(|suffix| suffix.strip_prefix("::"))
    else {
        return Ok(ImportResolution::NotBound);
    };
    resolve_member(index, (request, parent_module, associated), cancelled)
}

fn resolve_member<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str, &str),
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, module, name) = query;
    let scope = match module_file(index, (request, module), cancelled)? {
        ModuleLookup::Known(scope) => scope,
        ModuleLookup::Unproven => return Ok(ImportResolution::NotBound),
        ModuleLookup::Inaccessible => return Ok(ImportResolution::Unresolved),
    };
    let local = local_path(module);
    Ok(
        member_in_scope(index, (request, scope, name, local), cancelled)?
            .map_or(ImportResolution::NotBound, ImportResolution::Resolved),
    )
}

pub(super) fn resolve_in_scope<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, ModuleScope<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, scope, name) = query;
    if let Some((module, member)) = name.rsplit_once("::")
        && let ModuleLookup::Known(target_scope) =
            declared_module(index, (scope, module, request, false), cancelled)?
    {
        return member_in_scope(index, (request, target_scope, member, true), cancelled);
    }
    member_in_scope(index, (request, scope, name, true), cancelled)
}

fn member_in_scope<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, ModuleScope<'_>, &str, bool),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, scope, name, local) = query;
    let Some(qualified) = super::rust_inline_modules::qualified_name(scope.inline, name)? else {
        return Ok(None);
    };
    let candidates = resolution_candidates_for_file(index, &qualified, scope.file);
    let mut selected = Selection::default();
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.qualified_name == qualified
            && reference_kind_candidate(request.kind, candidate)
            && super::rust_path_visibility::visible(
                index,
                (request, scope, name, candidate),
                cancelled,
            )?
        {
            selected.retain(candidate);
        }
    }
    let provenance = if local {
        RUST_QUALIFIED_PATH_PROVENANCE
    } else {
        RUST_WORKSPACE_CRATE_PROVENANCE
    };
    Ok(selected
        .resolution(provenance, 1.0)
        .and_then(|resolution| resolution.target))
}

fn module_file<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<ModuleLookup<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, specifier) = query;
    let importing = request.file_path;
    let modules = &index.modules;
    let Some(root) = specifier.split("::").next() else {
        return Ok(ModuleLookup::Unproven);
    };
    let (entry, suffix, external) = if matches!(root, "crate" | "self" | "super") {
        let Some(file) = super::resolve_normalized_module_file(modules, importing, "rust") else {
            return Ok(ModuleLookup::Unproven);
        };
        let Some((entry, suffix)) = super::rust_root_ownership::starting_module(
            index,
            (file, request, specifier),
            cancelled,
        )?
        else {
            return Ok(ModuleLookup::Unproven);
        };
        (entry, suffix.to_owned(), false)
    } else {
        let Some(entry) = super::rust_dependency_paths::entry(index, (importing, root)) else {
            return Ok(ModuleLookup::Unproven);
        };
        let suffix = specifier
            .strip_prefix(root)
            .and_then(|suffix| suffix.strip_prefix("::"))
            .unwrap_or("");
        (
            ModuleScope {
                file: entry,
                inline: "",
                module: None,
            },
            suffix.to_owned(),
            true,
        )
    };
    declared_module(index, (entry, &suffix, request, external), cancelled)
}

fn declared_module<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (ModuleScope<'a>, &str, &ResolutionRequest<'_>, bool),
    cancelled: &mut Cancel,
) -> Result<ModuleLookup<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (mut current, suffix, request, external) = query;
    if suffix.is_empty() {
        return Ok(ModuleLookup::Known(current));
    }
    for component in suffix.split("::") {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let next = module_component(index, (current, component, request, external), cancelled)?;
        current = match next {
            ModuleLookup::Known(scope) => scope,
            _ => return Ok(next),
        };
    }
    Ok(ModuleLookup::Known(current))
}

fn module_component<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (ModuleScope<'a>, &str, &ResolutionRequest<'_>, bool),
    cancelled: &mut Cancel,
) -> Result<ModuleLookup<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (current, component, request, external) = query;
    let Some(name) = super::rust_inline_modules::qualified_name(current.inline, component)? else {
        return Ok(ModuleLookup::Unproven);
    };
    let edge = index
        .rust_paths
        .modules
        .get(current.file)
        .and_then(|edges| edges.get(&name));
    let Some(edge) = edge else {
        return Ok(inline_module(index, (current, component), cancelled)?
            .map_or(ModuleLookup::Unproven, ModuleLookup::Known));
    };
    let Some(edge) = edge else {
        return Ok(ModuleLookup::Unproven);
    };
    if edge.parent_module.as_ref() != current.module {
        return Ok(ModuleLookup::Unproven);
    }
    if !module_edge_visible(index, (current, request, external, edge.public), cancelled)? {
        return inaccessible_edge(index, (current, request), cancelled);
    }
    Ok(edge.file.as_ref().map_or(ModuleLookup::Unproven, |file| {
        ModuleLookup::Known(ModuleScope {
            file,
            inline: "",
            module: None,
        })
    }))
}

fn inaccessible_edge<'a, Cancel>(
    index: &ResolutionIndex,
    query: (ModuleScope<'a>, &ResolutionRequest<'_>),
    cancelled: &mut Cancel,
) -> Result<ModuleLookup<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (scope, request) = query;
    // Only the new nested declaration proof can establish a privacy violation.
    // Missing ownership remains an abstention to the existing base resolver.
    let proven = scope.module.is_some()
        && super::rust_root_ownership::root(index, scope.file).is_some()
        && super::rust_root_ownership::root(index, request.file_id).is_some()
        && super::rust_inline_modules::enclosing(index, request, cancelled)?.is_some();
    Ok(if proven {
        ModuleLookup::Inaccessible
    } else {
        ModuleLookup::Unproven
    })
}

fn inline_module<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (ModuleScope<'a>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ModuleScope<'a>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (scope, component) = query;
    let Some(name) = super::rust_inline_modules::qualified_name(scope.inline, component)? else {
        return Ok(None);
    };
    let candidate = select_candidate(
        resolution_candidates_for_file(index, &name, scope.file),
        |candidate| {
            (candidate.kind == SymbolKind::Module
                || super::qualtype_resolution::nominal_candidate(candidate.kind))
                && candidate.qualified_name == name
                && super::rust_inline_modules::owns(scope, candidate)
        },
        cancelled,
    )?;
    Ok(candidate
        .filter(|candidate| {
            candidate.kind == SymbolKind::Module && candidate.visibility == Some(Visibility::Public)
        })
        .map(|candidate| ModuleScope {
            file: scope.file,
            inline: &candidate.qualified_name,
            module: Some(&candidate.symbol_id),
        }))
}

fn module_edge_visible<Cancel>(
    index: &ResolutionIndex,
    query: (ModuleScope<'_>, &ResolutionRequest<'_>, bool, bool),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (declaring, request, external, public) = query;
    if public {
        return Ok(true);
    }
    if declaring.module.is_some() {
        return super::rust_path_visibility::declaration_visible(
            index,
            (request, declaring, None),
            cancelled,
        );
    }
    let importing = request.file_path;
    Ok(!external
        && index.modules.files.get(declaring.file).is_some_and(|file| {
            importing == file.path || rust_parent_module_contains(importing, &file.path)
        }))
}
