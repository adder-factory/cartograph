//! Index functions loaded by a proven top-level source edge once per script.
use super::{
    ExtractedImportBinding, FileId, HashMap, ImportBindingKind, ModuleFileMatch, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind, ReferenceResolution, ResolutionCandidate,
    ResolutionIndex, ResolutionIndexTarget, ResolutionRequest, ResolvedTarget, StageItemFailure,
    SymbolKind, module_file_match, normalize_relative_module_path, normalize_root_module_path,
    shell_command_language, size_of, try_clone_text, usize_to_u64,
};

use std::collections::HashSet;

type Functions = HashMap<String, Option<SourceFunction>>;

#[derive(Default)]
pub(super) struct SourceIndex {
    loads: HashMap<FileId, Option<HashMap<FileId, u64>>>,
    functions: HashMap<FileId, Functions>,
}

struct SourceFunction {
    target: ResolvedTarget,
    loaded_at: u64,
}

pub(super) fn metadata_binding(language: &str, binding: &ExtractedImportBinding) -> bool {
    shell_command_language(language)
        && binding.kind == ImportBindingKind::Namespace
        && binding.local_name == "*"
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !shell_command_language(&file.file.language) {
        return Ok(());
    }
    let mut loads = HashMap::<FileId, u64>::new();
    let mut proven = true;
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !metadata_binding(&file.file.language, binding) {
            continue;
        }
        let Some(loaded) = loaded_file(target.index, (file, binding)) else {
            proven = false;
            continue;
        };
        if let Some(position) = loads.get_mut(loaded) {
            *position = (*position).min(binding.span.end_byte());
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(FileId, u64)>() + loaded.as_str().len()),
        )?;
        loads.try_reserve(1).map_err(|_| StageItemFailure)?;
        loads.insert(loaded.clone(), binding.span.end_byte());
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(FileId, Option<HashMap<FileId, u64>>)>()
                    + file.file.file_id.as_str().len(),
            ),
    )?;
    target
        .index
        .languages
        .shell_sources
        .loads
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .languages
        .shell_sources
        .loads
        .insert(file.file.file_id.clone(), proven.then_some(loads));
    Ok(())
}

fn loaded_file<'a>(
    index: &'a ResolutionIndex,
    query: (&NativeFileFacts, &ExtractedImportBinding),
) -> Option<&'a FileId> {
    let (file, binding) = query;
    let language = &file.file.language;
    let script =
        normalize_relative_module_path(&file.file.normalized_path, &binding.module_specifier)?;
    let root = normalize_root_module_path(&binding.module_specifier);
    let script = module_file_match(
        index.modules.exact.get(&script),
        &index.modules.files,
        language,
    );
    let root = root.as_deref().map_or(ModuleFileMatch::Missing, |path| {
        module_file_match(
            index.modules.exact.get(path),
            &index.modules.files,
            language,
        )
    });
    match (script, root) {
        (ModuleFileMatch::Unique(a), ModuleFileMatch::Unique(b)) if a == b => Some(a),
        (ModuleFileMatch::Unique(a), ModuleFileMatch::Missing)
        | (ModuleFileMatch::Missing, ModuleFileMatch::Unique(a)) => Some(a),
        _ => None,
    }
}

pub(super) fn finalize<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let blocked = blocked_sources(target, cancelled)?;
    let functions = index_functions(target, cancelled)?;
    for (source, loads) in std::mem::take(&mut target.index.languages.shell_sources.loads) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if blocked.contains(&source) {
            continue;
        }
        let Some(loads) = loads else {
            continue;
        };
        let names = loaded_functions(target, (&functions, loads), cancelled)?;
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(FileId, Functions)>() + source.as_str().len()),
        )?;
        target
            .index
            .languages
            .shell_sources
            .functions
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        target
            .index
            .languages
            .shell_sources
            .functions
            .insert(source, names);
    }
    Ok(())
}

fn loaded_functions<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    input: (&HashMap<FileId, Functions>, HashMap<FileId, u64>),
    cancelled: &mut Cancel,
) -> Result<Functions, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (functions, loads) = input;
    let mut names = Functions::new();
    for (loaded, position) in loads {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(functions) = functions.get(&loaded) else {
            continue;
        };
        for (name, function) in functions {
            if cancelled() {
                return Err(StageItemFailure);
            }
            merge_function(target, (&mut names, name), (function.as_ref(), position))?;
        }
    }
    Ok(names)
}

struct LoadEffects {
    parents: HashMap<FileId, Vec<FileId>>,
    blocked: HashSet<FileId>,
    queue: Vec<FileId>,
}

fn blocked_sources<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<HashSet<FileId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let LoadEffects {
        parents,
        mut blocked,
        mut queue,
    } = load_effects(target, cancelled)?;
    while let Some(file) = queue.pop() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(parents) = parents.get(&file) else {
            continue;
        };
        for parent in parents {
            if cancelled() {
                return Err(StageItemFailure);
            }
            add_blocked((target.budget, &mut blocked, &mut queue), parent)?;
        }
    }
    Ok(blocked)
}

fn load_effects<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<LoadEffects, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut parents = HashMap::<FileId, Vec<FileId>>::new();
    let mut blocked = HashSet::new();
    let mut queue = Vec::new();
    for (source, loads) in &target.index.languages.shell_sources.loads {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(loads) = loads else {
            add_blocked((target.budget, &mut blocked, &mut queue), source)?;
            continue;
        };
        for loaded in loads.keys() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            add_parent((target.budget, &mut parents), (loaded, source))?;
        }
    }
    Ok(LoadEffects {
        parents,
        blocked,
        queue,
    })
}

fn add_blocked(
    state: (
        &mut super::ResolveBudget,
        &mut HashSet<FileId>,
        &mut Vec<FileId>,
    ),
    file: &FileId,
) -> Result<(), StageItemFailure> {
    let (budget, blocked, queue) = state;
    if blocked.contains(file) {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64((size_of::<FileId>() + file.as_str().len()) * 2),
    )?;
    blocked.try_reserve(1).map_err(|_| StageItemFailure)?;
    queue.try_reserve(1).map_err(|_| StageItemFailure)?;
    blocked.insert(file.clone());
    queue.push(file.clone());
    Ok(())
}

fn add_parent(
    state: (&mut super::ResolveBudget, &mut HashMap<FileId, Vec<FileId>>),
    edge: (&FileId, &FileId),
) -> Result<(), StageItemFailure> {
    let (budget, parents) = state;
    let (loaded, source) = edge;
    if !parents.contains_key(loaded) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(FileId, Vec<FileId>)>() + loaded.as_str().len()),
        )?;
        parents.try_reserve(1).map_err(|_| StageItemFailure)?;
        parents.insert(loaded.clone(), Vec::new());
    }
    budget.charge(usize_to_u64(size_of::<FileId>() + source.as_str().len()))?;
    let sources = parents.get_mut(loaded).ok_or(StageItemFailure)?;
    sources.try_reserve(1).map_err(|_| StageItemFailure)?;
    sources.push(source.clone());
    Ok(())
}

fn index_functions<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<HashMap<FileId, Functions>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut functions = HashMap::<FileId, Functions>::new();
    for (name, bucket) in &target.index.candidates {
        for candidate in bucket.as_slice() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if !source_function(target.index, (name, candidate)) {
                continue;
            }
            if !functions.contains_key(&candidate.file_id) {
                target.budget.charge(
                    RESOLUTION_MAP_NODE_ALLOWANCE
                        + usize_to_u64(
                            size_of::<(FileId, Functions)>() + candidate.file_id.as_str().len(),
                        ),
                )?;
                functions.try_reserve(1).map_err(|_| StageItemFailure)?;
                functions.insert(candidate.file_id.clone(), Functions::new());
            }
            let names = functions
                .get_mut(&candidate.file_id)
                .ok_or(StageItemFailure)?;
            if let Some(existing) = names.get_mut(name) {
                *existing = None;
                continue;
            }
            target
                .budget
                .charge(function_bytes(name, candidate.symbol_id.as_str()))?;
            names.try_reserve(1).map_err(|_| StageItemFailure)?;
            names.insert(
                try_clone_text(name)?,
                Some(SourceFunction {
                    target: ResolvedTarget {
                        symbol_id: candidate.symbol_id.clone(),
                        kind: candidate.kind,
                        confidence: 1.0,
                        provenance: "native-shell-source",
                    },
                    loaded_at: 0,
                }),
            );
        }
    }
    Ok(functions)
}

fn source_function(index: &ResolutionIndex, query: (&str, &ResolutionCandidate)) -> bool {
    let (name, candidate) = query;
    candidate.kind == SymbolKind::Function
        && candidate.top_level
        && candidate.qualified_name == name
        && index
            .modules
            .files
            .get(&candidate.file_id)
            .is_some_and(|file| shell_command_language(&file.language))
}

fn function_bytes(name: &str, symbol: &str) -> u64 {
    RESOLUTION_MAP_NODE_ALLOWANCE
        + usize_to_u64(size_of::<(String, Option<SourceFunction>)>() + name.len() + symbol.len())
}

fn merge_function(
    target: &mut ResolutionIndexTarget<'_>,
    query: (&mut Functions, &str),
    loaded: (Option<&SourceFunction>, u64),
) -> Result<(), StageItemFailure> {
    let (names, name) = query;
    let (function, position) = loaded;
    if let Some(existing) = names.get_mut(name) {
        if let (Some(existing), Some(function)) = (existing.as_mut(), function)
            && existing.target.symbol_id == function.target.symbol_id
        {
            existing.loaded_at = existing.loaded_at.min(position);
            return Ok(());
        }
        *existing = None;
        return Ok(());
    }
    target.budget.charge(function_bytes(
        name,
        function.map_or("", |f| f.target.symbol_id.as_str()),
    ))?;
    names.try_reserve(1).map_err(|_| StageItemFailure)?;
    names.insert(
        try_clone_text(name)?,
        function.map(|function| SourceFunction {
            target: copy_target(&function.target),
            loaded_at: position,
        }),
    );
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    if !shell_command_language(request.language) || request.kind != ReferenceKind::Calls {
        return Ok(None);
    }
    let function = index
        .languages
        .shell_sources
        .functions
        .get(request.file_id)
        .and_then(|names| names.get(request.name))
        .and_then(Option::as_ref)
        .filter(|function| function.loaded_at <= request.span.start_byte());
    Ok(function.map(|function| ReferenceResolution::resolved(copy_target(&function.target))))
}

fn copy_target(target: &ResolvedTarget) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: target.symbol_id.clone(),
        kind: target.kind,
        confidence: target.confidence,
        provenance: target.provenance,
    }
}
