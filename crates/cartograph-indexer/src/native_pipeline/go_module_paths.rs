//! Map import paths only within the nearest generation-owned go.mod boundary.
use super::{
    ExtractedImportBinding, HashMap, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceDispatch, ReferenceKind, ReferenceResolution, ResolutionCandidate, ResolutionIndex,
    ResolutionIndexTarget, ResolutionRequest, StageItemFailure, SymbolKind,
    receiver_resolution::ReceiverQuery, size_of, try_clone_text, usize_to_u64,
};

const MODULE_ID_PREFIX: &str = "go.module:";
const MAX_MODULE_ANCESTORS: usize = 64;
const SITE_METADATA: &str = "<go-package-site>";

#[derive(Default)]
pub(super) struct ModuleIndex {
    roots: HashMap<String, Option<String>>,
    packages: HashMap<String, Option<String>>,
    sites: HashMap<super::FileId, SiteMap>,
}

type SiteMap = HashMap<(u64, u64, ReferenceKind), Option<String>>;

pub(super) fn metadata_binding(language: &str, binding: &ExtractedImportBinding) -> bool {
    language == "go" && binding.module_specifier == SITE_METADATA
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let path = file.file.normalized_path.as_str();
    if file.file.language != "go" {
        return Ok(());
    }
    index_sites(target, file, cancelled)?;
    if path.rsplit('/').next() != Some("go.mod") {
        return index_package(target, file);
    }
    let root = path.rsplit_once('/').map_or("", |(root, _)| root);
    let mut module = None;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind == SymbolKind::Module {
            module = symbol.input.qualified_name.strip_prefix(MODULE_ID_PREFIX);
        }
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(size_of::<(String, Option<String>)>() + root.len())
            + module.map_or(0, |module| usize_to_u64(module.len())),
    )?;
    let roots = &mut target.index.go_modules.roots;
    roots.try_reserve(1).map_err(|_| StageItemFailure)?;
    roots.insert(
        try_clone_text(root)?,
        module.map(try_clone_text).transpose()?,
    );
    Ok(())
}

fn index_package(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
) -> Result<(), StageItemFailure> {
    let context = target
        .index
        .modules
        .files
        .get(&file.file.file_id)
        .ok_or(StageItemFailure)?;
    if context.path.ends_with("_test.go") {
        return Ok(());
    }
    let packages = &mut target.index.go_modules.packages;
    if let Some(existing) = packages.get_mut(&context.directory) {
        if existing.as_deref() != context.package.as_deref() {
            *existing = None;
        }
        return Ok(());
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(String, Option<String>)>()
                    + context.directory.len()
                    + context.package.as_ref().map_or(0, String::len),
            ),
    )?;
    packages.try_reserve(1).map_err(|_| StageItemFailure)?;
    packages.insert(
        try_clone_text(&context.directory)?,
        context.package.as_deref().map(try_clone_text).transpose()?,
    );
    Ok(())
}

fn index_sites<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut sites = SiteMap::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !metadata_binding("go", binding) {
            continue;
        }
        let Some(key) = site_key(binding) else {
            continue;
        };
        if let Some(existing) = sites.get_mut(&key) {
            if existing.as_deref() != Some(&binding.imported_name) {
                *existing = None;
            }
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(
                    size_of::<((u64, u64, ReferenceKind), Option<String>)>()
                        + binding.imported_name.len(),
                ),
        )?;
        sites.try_reserve(1).map_err(|_| StageItemFailure)?;
        sites.insert(key, Some(try_clone_text(&binding.imported_name)?));
    }
    if sites.is_empty() {
        return Ok(());
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(super::FileId, SiteMap)>() + file.file.file_id.as_str().len(),
            ),
    )?;
    target
        .index
        .go_modules
        .sites
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .go_modules
        .sites
        .insert(file.file.file_id.clone(), sites);
    Ok(())
}

fn site_key(binding: &ExtractedImportBinding) -> Option<(u64, u64, ReferenceKind)> {
    [
        ReferenceKind::FieldAccess,
        ReferenceKind::TypeOf,
        ReferenceKind::Returns,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == binding.local_name)
    .map(|kind| (binding.span.start_byte(), binding.span.end_byte(), kind))
}

pub(super) fn prefer<Cancel>(
    index: &ResolutionIndex,
    query: (ReferenceResolution, ReceiverQuery<'_, '_>),
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (
        base,
        ReceiverQuery {
            context,
            reference,
            import_binding_scratch,
        },
    ) = query;
    if base.target.is_some() || context.identity.language != "go" {
        return Ok(base);
    }
    let Some(name) = index
        .go_modules
        .sites
        .get(&context.identity.file_id)
        .and_then(|sites| {
            sites.get(&(
                reference.span.start_byte(),
                reference.span.end_byte(),
                reference.kind,
            ))
        })
        .and_then(Option::as_deref)
    else {
        return Ok(base);
    };
    let request = ResolutionRequest {
        file_id: &context.identity.file_id,
        file_path: &context.identity.path,
        language: "go",
        import_bindings: import_binding_scratch.select(context.import_bindings, name),
        owner: reference.owner.as_ref(),
        name,
        dispatch: ReferenceDispatch::Static,
        kind: reference.kind,
        span: reference.span,
    };
    Ok(super::go_path_resolution::resolve(index, &request, cancelled)?.unwrap_or(base))
}

pub(super) fn matches(
    index: &ResolutionIndex,
    query: (
        &ResolutionRequest<'_>,
        &ResolutionCandidate,
        &ExtractedImportBinding,
    ),
) -> bool {
    let (request, candidate, binding) = query;
    let Some(source) = index.modules.files.get(request.file_id) else {
        return false;
    };
    let Some(target) = index.modules.files.get(&candidate.file_id) else {
        return false;
    };
    if target.path.ends_with("_test.go")
        || index
            .go_modules
            .packages
            .get(&target.directory)
            .and_then(Option::as_ref)
            .is_none_or(|package| Some(package) != target.package.as_ref())
    {
        return false;
    }
    let Some((root, Some(module))) = owning_module(index, &source.directory) else {
        return false;
    };
    let suffix = binding
        .module_specifier
        .strip_prefix(module)
        .and_then(|suffix| suffix.strip_prefix('/'));
    let directory = if root.is_empty() {
        Some(target.directory.as_str())
    } else {
        target
            .directory
            .strip_prefix(root)
            .and_then(|path| path.strip_prefix('/'))
    };
    suffix.is_some_and(|path| Some(path) == directory)
        && owning_module(index, &target.directory).is_some_and(|(owner, _)| owner == root)
}

fn owning_module<'index>(
    index: &'index ResolutionIndex,
    mut directory: &str,
) -> Option<(&'index str, Option<&'index str>)> {
    for _ in 0..MAX_MODULE_ANCESTORS {
        if let Some((root, module)) = index.go_modules.roots.get_key_value(directory) {
            return Some((root, module.as_deref()));
        }
        if directory.is_empty() {
            return None;
        }
        directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
    None
}
