//! Bounded Python module paths and explicitly imported top-level members.
//! Source roots inferred from package layout remain convention evidence;
//! competing paths, declarations, and symbol/submodule bindings abstain.

use super::{
    FileId, HashMap, ImportBindingKind, ImportResolution, ImportResolutionRequest, ModuleFileMatch,
    ModulePathIndex, ModuleResolutionAttempt, ModuleResolutionRequest,
    RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, ResolvedTarget, SourceLanguage, StageItemFailure, SymbolKind,
    import_binding_target, module_file_match, normalize_relative_module_path,
    reference_kind_candidate, resolution_candidates_for_file, size_of, strip_module_extension,
    try_clone_text, usize_to_u64,
};

const MAXIMUM_SPECIFIER_BYTES: usize = 4_096;
const SOURCE_ROOT_PROVENANCE: &str = "native-python-source-root";
const SOURCE_ROOT_CONFIDENCE: f32 = 0.9;
const STDLIB_PREFIXES: [&str; 10] = [
    "os",
    "sys",
    "json",
    "re",
    "math",
    "datetime",
    "collections",
    "typing",
    "pathlib",
    "logging",
];

#[derive(Default)]
pub(super) struct SourceRootIndex {
    modules: HashMap<String, Option<FileId>>,
}

#[derive(Clone, Copy)]
struct ModuleTarget<'a> {
    file_id: &'a FileId,
    source_root: bool,
}

#[derive(Clone, Copy)]
enum ModuleLookup<'a> {
    Missing,
    Unique(ModuleTarget<'a>),
    Ambiguous,
}

enum MemberLookup<'a> {
    Missing,
    Unique(&'a ResolutionCandidate),
    Ambiguous,
}

#[derive(Clone, Copy)]
pub(super) struct ImportQuery<'a, 'b> {
    pub(super) index: &'a ResolutionIndex,
    pub(super) input: ImportResolutionRequest<'a, 'b>,
    pub(super) binding: &'a super::ExtractedImportBinding,
}

/// Prepare one indexed alias per Python file beneath a detected package root
/// or the conventional `src/` root. Both memory and spill preparation call it.
pub(super) fn index_source_roots<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut aliases = SourceRootIndex::default();
    for (file_id, file) in &target.index.modules.files {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if file.language != SourceLanguage::Python.as_str() {
            continue;
        }
        let root = detected_source_root(&target.index.modules, &file.path);
        let Some(relative) = root.and_then(|root| file.path.strip_prefix(root)) else {
            continue;
        };
        let Some(stem) = strip_module_extension(relative, &file.language) else {
            continue;
        };
        let key = stem.strip_suffix("/__init__").unwrap_or(stem);
        insert_source_alias(&mut aliases, (key, file_id), target.budget)?;
    }
    target.index.modules.python_source_roots = aliases;
    Ok(())
}

fn detected_source_root<'a>(modules: &ModulePathIndex, path: &'a str) -> Option<&'a str> {
    for (separator, _) in path.match_indices('/') {
        let directory = &path[..separator];
        let package = module_file_match(
            modules.directory_index.get(directory),
            &modules.files,
            SourceLanguage::Python.as_str(),
        );
        if !matches!(package, ModuleFileMatch::Missing) {
            return directory.rfind('/').map(|parent| &path[..=parent]);
        }
    }
    path.starts_with("src/").then_some("src/")
}

fn insert_source_alias(
    aliases: &mut SourceRootIndex,
    entry: (&str, &FileId),
    budget: &mut super::ResolveBudget,
) -> Result<(), StageItemFailure> {
    let (key, file_id) = entry;
    if let Some(existing) = aliases.modules.get_mut(key) {
        if existing.as_ref() != Some(file_id) {
            *existing = None;
        }
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(String, Option<FileId>)>()))
            .saturating_add(usize_to_u64(key.len()))
            .saturating_add(usize_to_u64(file_id.as_str().len())),
    )?;
    aliases
        .modules
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    aliases
        .modules
        .insert(try_clone_text(key)?, Some(file_id.clone()));
    Ok(())
}

pub(super) fn resolve_module_file<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    if request.importing_language != SourceLanguage::Python.as_str() {
        return ModuleResolutionAttempt::NotMatched;
    }
    match module_lookup(modules, request) {
        ModuleLookup::Unique(target) => ModuleResolutionAttempt::Resolved(target.file_id),
        ModuleLookup::Missing | ModuleLookup::Ambiguous => ModuleResolutionAttempt::Rejected,
    }
}

fn module_lookup<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleLookup<'a> {
    let Some(path) = module_path(request) else {
        return ModuleLookup::Missing;
    };
    let absolute = !request.specifier.starts_with('.');
    for (separator, _) in path.match_indices('/') {
        match path_lookup(modules, &path[..separator], absolute) {
            ModuleLookup::Ambiguous => return ModuleLookup::Ambiguous,
            ModuleLookup::Unique(target) => {
                return package_descendant_lookup(modules, target, &path[separator + 1..]);
            }
            ModuleLookup::Missing => {}
        }
    }
    path_lookup(modules, &path, absolute)
}

fn package_descendant_lookup<'a>(
    modules: &'a ModulePathIndex,
    parent: ModuleTarget<'a>,
    suffix: &str,
) -> ModuleLookup<'a> {
    if !is_package(modules, parent.file_id) {
        return ModuleLookup::Ambiguous;
    }
    if suffix.is_empty() || !suffix.split('/').all(identifier) {
        return ModuleLookup::Missing;
    }
    let Some(package) = modules.files.get(parent.file_id) else {
        return ModuleLookup::Missing;
    };
    let path = if package.directory.is_empty() {
        suffix.to_owned()
    } else {
        format!("{}/{suffix}", package.directory)
    };
    let prefix_length = path.len() - suffix.len();
    for (separator, _) in suffix.match_indices('/') {
        match path_lookup(modules, &path[..prefix_length + separator], false) {
            ModuleLookup::Ambiguous => return ModuleLookup::Ambiguous,
            ModuleLookup::Unique(target) if !is_package(modules, target.file_id) => {
                return ModuleLookup::Ambiguous;
            }
            ModuleLookup::Missing | ModuleLookup::Unique(_) => {}
        }
    }
    match path_lookup(modules, &path, false) {
        ModuleLookup::Unique(mut target) => {
            target.source_root = parent.source_root;
            ModuleLookup::Unique(target)
        }
        other => other,
    }
}

fn module_path(request: ModuleResolutionRequest<'_>) -> Option<String> {
    let specifier = request.specifier;
    if specifier.is_empty() || specifier.len() > MAXIMUM_SPECIFIER_BYTES {
        return None;
    }
    if specifier.starts_with('.') {
        let relative = relative_specifier(specifier);
        let entry = format!("{}/__init__", relative.trim_end_matches('/'));
        return normalize_relative_module_path(request.importing_path, &entry).and_then(|path| {
            path.strip_suffix("/__init__")
                .map(str::to_owned)
                .or_else(|| (path == "__init__").then(String::new))
        });
    }
    let valid = specifier.split(['.', '/']).all(identifier);
    let leading = specifier.split(['.', '/']).next()?;
    (valid && !STDLIB_PREFIXES.contains(&leading)).then(|| specifier.replace('.', "/"))
}

fn relative_specifier(specifier: &str) -> String {
    if specifier.starts_with("./") || specifier.starts_with("../") {
        return specifier.to_owned();
    }
    let dots = specifier.bytes().take_while(|byte| *byte == b'.').count();
    let prefix = if dots == 1 {
        "./".to_owned()
    } else {
        "../".repeat(dots - 1)
    };
    format!("{prefix}{}", specifier[dots..].replace('.', "/"))
}

fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && chars.all(|character| character == '_' || character.is_alphanumeric())
}

fn path_lookup<'a>(modules: &'a ModulePathIndex, path: &str, absolute: bool) -> ModuleLookup<'a> {
    let stem = if path.is_empty() { "__init__" } else { path };
    let file = module_file_match(modules.stem.get(stem), &modules.files, "python");
    let package = module_file_match(modules.directory_index.get(path), &modules.files, "python");
    let direct = match (file, package) {
        (ModuleFileMatch::Missing, ModuleFileMatch::Missing) => ModuleLookup::Missing,
        (ModuleFileMatch::Unique(file_id), ModuleFileMatch::Missing)
        | (ModuleFileMatch::Missing, ModuleFileMatch::Unique(file_id)) => {
            ModuleLookup::Unique(ModuleTarget {
                file_id,
                source_root: false,
            })
        }
        _ => ModuleLookup::Ambiguous,
    };
    if !absolute {
        return direct;
    }
    match (direct, modules.python_source_roots.modules.get(path)) {
        (ModuleLookup::Missing, Some(Some(file_id))) => ModuleLookup::Unique(ModuleTarget {
            file_id,
            source_root: true,
        }),
        (selected, None) => selected,
        _ => ModuleLookup::Ambiguous,
    }
}

fn is_package(modules: &ModulePathIndex, file_id: &FileId) -> bool {
    modules.files.get(file_id).is_some_and(|file| {
        file.path.ends_with("/__init__.py")
            || file.path.ends_with("/__init__.pyi")
            || matches!(file.path.as_str(), "__init__.py" | "__init__.pyi")
    })
}

pub(super) fn resolve_import<Cancel>(
    query: ImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if query.input.reference.import_bindings.fallback_blocked {
        return Ok(ImportResolution::Unresolved);
    }
    match query.binding.kind {
        ImportBindingKind::Named => resolve_named_import(query, cancelled),
        ImportBindingKind::Namespace => resolve_namespace_import(query, cancelled),
        _ => Ok(ImportResolution::Unresolved),
    }
}

fn resolve_named_import<Cancel>(
    query: ImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let module = module_lookup(&query.index.modules, module_request(query));
    let member = match module {
        ModuleLookup::Ambiguous => return Ok(ImportResolution::Unresolved),
        ModuleLookup::Unique(target) => find_member(
            query.index,
            (target.file_id, &query.binding.imported_name),
            cancelled,
        )?,
        ModuleLookup::Missing => MemberLookup::Missing,
    };
    let child = child_module(query, module);
    match (member, child) {
        (MemberLookup::Unique(candidate), ModuleLookup::Missing) => {
            if query.input.reference.name != query.binding.local_name
                && !matches!(query.input.site, super::ImportReferenceSite::Declaration)
            {
                return Ok(ImportResolution::Unresolved);
            }
            let ModuleLookup::Unique(target) = module else {
                return Err(StageItemFailure);
            };
            Ok(member_resolution(query, (candidate, target.source_root)))
        }
        (MemberLookup::Missing, ModuleLookup::Unique(target)) => {
            resolve_module_member(query, target, cancelled)
        }
        (MemberLookup::Missing, ModuleLookup::Missing) => Ok(missing_resolution(query, module)),
        _ => Ok(ImportResolution::Unresolved),
    }
}

fn child_module<'a>(query: ImportQuery<'a, '_>, parent: ModuleLookup<'a>) -> ModuleLookup<'a> {
    match parent {
        ModuleLookup::Unique(target) if is_package(&query.index.modules, target.file_id) => {
            return package_descendant_lookup(
                &query.index.modules,
                target,
                &query.binding.imported_name,
            );
        }
        ModuleLookup::Unique(_) => return ModuleLookup::Missing,
        ModuleLookup::Ambiguous => return ModuleLookup::Ambiguous,
        ModuleLookup::Missing => {}
    }
    let specifier = format!(
        "{}/{}",
        query.binding.module_specifier, query.binding.imported_name
    );
    module_lookup(
        &query.index.modules,
        ModuleResolutionRequest {
            specifier: &specifier,
            ..module_request(query)
        },
    )
}

fn resolve_namespace_import<Cancel>(
    query: ImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let module = module_lookup(&query.index.modules, module_request(query));
    if query.binding.module_specifier.starts_with('.') {
        return resolve_relative_binding(query, module, cancelled);
    }
    // Extraction does not distinguish `import a.b` from `import a.b as a`.
    // Those bindings expose different attributes, so neither is guessed.
    if query.binding.module_specifier.contains('.')
        && query.binding.module_specifier.split('.').next() == Some(&query.binding.local_name)
    {
        return Ok(ImportResolution::Unresolved);
    }
    match module {
        ModuleLookup::Unique(target) => resolve_module_member(query, target, cancelled),
        other => Ok(missing_resolution(query, other)),
    }
}

fn resolve_relative_binding<Cancel>(
    query: ImportQuery<'_, '_>,
    module: ModuleLookup<'_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some((parent, name)) = query.binding.module_specifier.rsplit_once('/') else {
        return Ok(ImportResolution::Unresolved);
    };
    let package = module_lookup(
        &query.index.modules,
        ModuleResolutionRequest {
            specifier: parent,
            ..module_request(query)
        },
    );
    let member = match package {
        ModuleLookup::Unique(target) => {
            find_member(query.index, (target.file_id, name), cancelled)?
        }
        ModuleLookup::Ambiguous => return Ok(ImportResolution::Unresolved),
        ModuleLookup::Missing => MemberLookup::Missing,
    };
    match (member, module) {
        (MemberLookup::Unique(candidate), ModuleLookup::Missing)
            if query.input.reference.name == query.binding.local_name
                || query.input.reference.kind == super::ReferenceKind::Imports =>
        {
            Ok(member_resolution(query, (candidate, false)))
        }
        (MemberLookup::Missing, ModuleLookup::Unique(target)) => {
            resolve_module_member(query, target, cancelled)
        }
        _ => Ok(ImportResolution::Unresolved),
    }
}

fn resolve_module_member<Cancel>(
    query: ImportQuery<'_, '_>,
    module: ModuleTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if query.input.reference.kind == super::ReferenceKind::Imports
        || (query.input.reference.kind == super::ReferenceKind::References
            && (query.input.reference.name == query.binding.local_name
                || matches!(query.input.site, super::ImportReferenceSite::Declaration)))
    {
        return module_file_resolution(query.index, module);
    }
    let Some(member) = query
        .input
        .reference
        .name
        .strip_prefix(&query.binding.local_name)
        .and_then(|suffix| suffix.strip_prefix('.'))
        .filter(|name| identifier(name))
    else {
        return Ok(ImportResolution::Unresolved);
    };
    match find_member(query.index, (module.file_id, member), cancelled)? {
        MemberLookup::Unique(candidate) => {
            Ok(member_resolution(query, (candidate, module.source_root)))
        }
        MemberLookup::Missing | MemberLookup::Ambiguous => Ok(ImportResolution::Unresolved),
    }
}

fn find_member<'a, Cancel>(
    index: &'a ResolutionIndex,
    member: (&FileId, &str),
    cancelled: &mut Cancel,
) -> Result<MemberLookup<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file_id, name) = member;
    let mut matched = None;
    for candidate in resolution_candidates_for_file(index, name, file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !candidate.top_level
            || candidate.parent_symbol_id.is_some()
            || candidate.qualified_name != name
            || candidate.augmentation
        {
            continue;
        }
        if matched.is_some() {
            return Ok(MemberLookup::Ambiguous);
        }
        matched = Some(candidate);
    }
    Ok(matched.map_or(MemberLookup::Missing, MemberLookup::Unique))
}

fn member_resolution(
    query: ImportQuery<'_, '_>,
    member: (&ResolutionCandidate, bool),
) -> ImportResolution {
    let (candidate, source_root) = member;
    if matches!(candidate.kind, SymbolKind::Import | SymbolKind::File)
        || !reference_kind_candidate(query.input.reference.kind, candidate)
    {
        return ImportResolution::Unresolved;
    }
    ImportResolution::Resolved(with_source_root(
        import_binding_target(candidate),
        source_root,
    ))
}

fn with_source_root(mut target: ResolvedTarget, source_root: bool) -> ResolvedTarget {
    if source_root {
        target.confidence = SOURCE_ROOT_CONFIDENCE;
        target.provenance = SOURCE_ROOT_PROVENANCE;
    }
    target
}

fn module_request<'a>(query: ImportQuery<'a, '_>) -> ModuleResolutionRequest<'a> {
    ModuleResolutionRequest {
        importing_path: query.input.reference.file_path,
        specifier: &query.binding.module_specifier,
        importing_language: SourceLanguage::Python.as_str(),
    }
}

fn missing_resolution(query: ImportQuery<'_, '_>, module: ModuleLookup<'_>) -> ImportResolution {
    if query.binding.module_specifier.starts_with('.') || !matches!(module, ModuleLookup::Missing) {
        ImportResolution::Unresolved
    } else {
        ImportResolution::NotBound
    }
}

pub(super) fn resolve_module_reference<Cancel>(
    index: &ResolutionIndex,
    reference: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    for binding in reference.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind == ImportBindingKind::Namespace
            && binding.module_specifier.starts_with('.')
            && binding.module_specifier == reference.name
            && binding.span.start_byte() >= reference.span.start_byte()
            && binding.span.end_byte() <= reference.span.end_byte()
        {
            return resolve_import(
                ImportQuery {
                    index,
                    input: ImportResolutionRequest {
                        reference,
                        site: super::ImportReferenceSite::Declaration,
                    },
                    binding,
                },
                cancelled,
            );
        }
    }
    let module = module_lookup(
        &index.modules,
        ModuleResolutionRequest {
            importing_path: reference.file_path,
            specifier: reference.name,
            importing_language: reference.language,
        },
    );
    let ModuleLookup::Unique(target) = module else {
        return Ok(ImportResolution::Unresolved);
    };
    module_file_resolution(index, target)
}

fn module_file_resolution(
    index: &ResolutionIndex,
    target: ModuleTarget<'_>,
) -> Result<ImportResolution, StageItemFailure> {
    let symbol_id = index
        .file_symbols
        .get(target.file_id)
        .ok_or(StageItemFailure)?;
    Ok(ImportResolution::Resolved(with_source_root(
        ResolvedTarget {
            symbol_id: symbol_id.clone(),
            kind: SymbolKind::File,
            confidence: super::IMPORT_BINDING_CONFIDENCE,
            provenance: super::MODULE_IMPORT_PROVENANCE,
        },
        target.source_root,
    )))
}
