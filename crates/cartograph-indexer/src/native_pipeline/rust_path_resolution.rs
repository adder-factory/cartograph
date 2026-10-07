//! Exact Rust imports and paths rooted in the importing crate or an indexed
//! workspace crate. Explicit bindings take precedence over wildcard imports.

use super::{
    FileId, HashMap, IMPORT_BINDING_PROVENANCE, ImportBindingKind, ModulePathIndex,
    NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, RUST_QUALIFIED_PATH_PROVENANCE,
    RUST_WORKSPACE_CRATE_PROVENANCE, ReferenceDispatch, ReferenceResolution, ResolutionIndex,
    ResolutionIndexTarget, ResolutionRequest, ResolvedTarget, RustCandidateVisibility,
    StageItemFailure, SymbolKind, Visibility, normalize_relative_module_path,
    reference_kind_candidate, resolution_candidates_for_file, resolve_normalized_module_file,
    rust_inline_modules::ModuleScope, rust_module_candidate_visible, rust_parent_module_contains,
    select_candidate, size_of, try_clone_text, usize_to_u64,
};

const MAXIMUM_PATH_BYTES: usize = 1_024;

enum ExplicitPath {
    Absent,
    Unique(String),
    Ambiguous,
}

#[derive(Default)]
pub(super) struct PathIndex {
    pub(super) dependencies: super::rust_dependency_paths::DependencyIndex,
    pub(super) modules: HashMap<FileId, HashMap<String, Option<ModuleEdge>>>,
    pub(super) roots: super::rust_root_ownership::RootIndex,
    pub(super) facades: super::rust_facade_resolution::FacadeIndex,
    pub(super) use_scopes: super::rust_use_bindings::ImplScopes,
}

pub(super) struct ModuleEdge {
    pub(super) file: Option<FileId>,
    pub(super) public: bool,
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    super::rust_dependency_paths::index_file(target, file, cancelled)?;
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
    if request.language != "rust"
        || request.name.len() > MAXIMUM_PATH_BYTES
        || request.dispatch != ReferenceDispatch::Static
    {
        return Ok(None);
    }
    let explicit = match explicit_path(index, request, cancelled)? {
        ExplicitPath::Absent => None,
        ExplicitPath::Unique(path) => Some(path),
        ExplicitPath::Ambiguous => return Ok(None),
    };
    let path = explicit.as_deref().unwrap_or(request.name);
    let root = path.split("::").next().unwrap_or(path);
    if !rooted_path(&index.modules, path)
        && super::rust_dependency_paths::entry(index, (request.file_path, root)).is_none()
    {
        return Ok(None);
    }
    let mut target = match resolve_path(index, (request, path), cancelled)? {
        Some(target) => Some(target),
        None => super::rust_facade_resolution::resolve(index, (request, path), cancelled)?,
    };
    if explicit.is_some()
        && path
            .split_once("::")
            .is_some_and(|(root, _)| matches!(root, "crate" | "self" | "super"))
        && let Some(target) = &mut target
    {
        target.provenance = IMPORT_BINDING_PROVENANCE;
    }
    Ok(target.map(ReferenceResolution::resolved))
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
        if binding.kind != ImportBindingKind::Namespace || binding.local_name == "*" {
            continue;
        }
        let Some(suffix) = request
            .name
            .strip_prefix(&binding.local_name)
            .filter(|suffix| suffix.is_empty() || suffix.starts_with("::"))
        else {
            continue;
        };
        if !super::rust_use_bindings::allows(index, (request, binding), cancelled)? {
            return Ok(ExplicitPath::Absent);
        }
        if retained.is_some()
            || binding.module_specifier.len().saturating_add(suffix.len()) > MAXIMUM_PATH_BYTES
        {
            return Ok(ExplicitPath::Ambiguous);
        }
        retained = Some(format!("{}{suffix}", binding.module_specifier));
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
    let (request, path) = query;
    let Some((module, name)) = path.rsplit_once("::") else {
        return Ok(None);
    };
    if let Some(target) = resolve_member(index, (request, module, name), cancelled)? {
        return Ok(Some(target));
    }
    let Some((parent_module, _)) = module.rsplit_once("::") else {
        return Ok(None);
    };
    let Some(associated) = path
        .strip_prefix(parent_module)
        .and_then(|suffix| suffix.strip_prefix("::"))
    else {
        return Ok(None);
    };
    resolve_member(index, (request, parent_module, associated), cancelled)
}

fn resolve_member<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, module, name) = query;
    let Some(scope) = module_file(index, (request, module), cancelled)? else {
        return Ok(None);
    };
    let Some(qualified) = super::rust_inline_modules::qualified_name(scope.inline, name)? else {
        return Ok(None);
    };
    let candidates = resolution_candidates_for_file(index, &qualified, scope.file);
    let root = module.split("::").next().unwrap_or(module);
    let local = matches!(root, "crate" | "self" | "super");
    let scoped_access =
        local && super::rust_inline_modules::contains_owner(index, (request, scope));
    let candidate = select_candidate(
        candidates,
        |candidate| {
            candidate.qualified_name == qualified
                && reference_kind_candidate(request.kind, candidate)
                && (scoped_access || super::rust_use_bindings::visible(index, request, candidate))
                && if scope.inline.is_empty() {
                    rust_module_candidate_visible(RustCandidateVisibility {
                        index,
                        candidate,
                        target_name: name,
                        source_path: request.file_path,
                    })
                } else {
                    super::rust_inline_modules::owns(scope, candidate)
                        && (local || candidate.visibility == Some(Visibility::Public))
                }
        },
        cancelled,
    )?;
    let provenance = if local {
        RUST_QUALIFIED_PATH_PROVENANCE
    } else {
        RUST_WORKSPACE_CRATE_PROVENANCE
    };
    Ok(candidate.map(|candidate| ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: 1.0,
        provenance,
    }))
}

fn module_file<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ModuleScope<'a>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, specifier) = query;
    let importing = request.file_path;
    let modules = &index.modules;
    let Some(root) = specifier.split("::").next() else {
        return Ok(None);
    };
    let (entry, suffix, external) = if matches!(root, "crate" | "self" | "super") {
        let Some(file) = super::resolve_normalized_module_file(modules, importing, "rust") else {
            return Ok(None);
        };
        let Some((entry, suffix)) = super::rust_root_ownership::starting_module(
            index,
            (file, request, specifier),
            cancelled,
        )?
        else {
            return Ok(None);
        };
        (entry, suffix.to_owned(), false)
    } else {
        let Some(entry) = super::rust_dependency_paths::entry(index, (importing, root)) else {
            return Ok(None);
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
    declared_module(index, (entry, &suffix, importing, external), cancelled)
}

fn declared_module<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (ModuleScope<'a>, &str, &str, bool),
    cancelled: &mut Cancel,
) -> Result<Option<ModuleScope<'a>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (mut current, suffix, importing, external) = query;
    if suffix.is_empty() {
        return Ok(Some(current));
    }
    for component in suffix.split("::") {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !identifier(component) {
            return Ok(None);
        }
        let edge = current
            .inline
            .is_empty()
            .then(|| {
                index
                    .rust_paths
                    .modules
                    .get(current.file)
                    .and_then(|edges| edges.get(component))
            })
            .flatten();
        let Some(edge) = edge else {
            let Some(scope) = inline_module(index, (current, component), cancelled)? else {
                return Ok(None);
            };
            current = scope;
            continue;
        };
        let Some(edge) = edge else { return Ok(None) };
        if !module_edge_visible(index, (current.file, importing, external), edge.public) {
            return Ok(None);
        }
        let Some(file) = edge.file.as_ref() else {
            return Ok(None);
        };
        current = ModuleScope {
            file,
            inline: "",
            module: None,
        };
    }
    Ok(Some(current))
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
            candidate.kind == SymbolKind::Module
                && candidate.qualified_name == name
                && super::rust_inline_modules::owns(scope, candidate)
                // File visibility cannot prove access through a private inline
                // child. Keep traversal in the public subset of the facade.
                && candidate.visibility == Some(Visibility::Public)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| ModuleScope {
        file: scope.file,
        inline: &candidate.qualified_name,
        module: Some(&candidate.symbol_id),
    }))
}

fn module_edge_visible(
    index: &ResolutionIndex,
    query: (&FileId, &str, bool),
    public: bool,
) -> bool {
    let (declaring, importing, external) = query;
    public
        || !external
            && index.modules.files.get(declaring).is_some_and(|file| {
                importing == file.path || rust_parent_module_contains(importing, &file.path)
            })
}

fn identifier(component: &str) -> bool {
    !component.is_empty()
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
