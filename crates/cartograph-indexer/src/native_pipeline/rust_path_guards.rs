//! Private Rust identity and ambiguity evidence; unsupported paths stop here.
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use super::{
    FileId, NativeFileFacts, NativeSymbolFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceDispatch,
    ReferenceResolution, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest,
    StageItemFailure, SymbolKind, size_of, usize_to_u64,
};

#[derive(Default)]
pub(super) struct Declarations {
    modules: HashMap<FileId, HashMap<String, bool>>,
    standard_files: HashSet<FileId>,
    standard_roots: HashSet<FileId>,
    raw_candidates: super::CandidateMap,
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
    let mut modules = HashMap::new();
    let mut standard = false;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let name = declaration_name(symbol);
        standard |= name.is_some_and(|name| matches!(name, "std" | "core"));
        if let Some(name) = name
            && (symbol.kind == SymbolKind::Module
                || symbol.name.starts_with("./")
                || super::qualtype_resolution::nominal_candidate(symbol.kind))
        {
            retain_module(target, (&mut modules, symbol, name))?;
        }
        retain_raw_aliases(target, (file, symbol))?;
    }
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        standard |= !super::rust_use_bindings::metadata_binding("rust", binding)
            && matches!(raw_name(&binding.local_name), "std" | "core");
    }
    if standard {
        retain_file(
            target.budget,
            (
                &mut target.index.rust_paths.declarations.standard_files,
                &file.file.file_id,
            ),
        )?;
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(FileId, HashMap<String, bool>)>() + file.file.file_id.as_str().len(),
            ),
    )?;
    let declarations = &mut target.index.rust_paths.declarations.modules;
    declarations.try_reserve(1).map_err(|_| StageItemFailure)?;
    declarations.insert(file.file.file_id.clone(), modules);
    Ok(())
}

pub(super) fn declaration_name(symbol: &NativeSymbolFacts) -> Option<&str> {
    if matches!(
        symbol.kind,
        SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Field | SymbolKind::Property
    ) {
        return None;
    }
    let name = if symbol.kind == SymbolKind::Import {
        symbol.name.strip_prefix("./")?.rsplit('/').next()?
    } else {
        symbol.name.as_str()
    };
    Some(name.strip_prefix("r#").unwrap_or(name))
}

fn retain_module(
    target: &mut ResolutionIndexTarget<'_>,
    query: (&mut HashMap<String, bool>, &NativeSymbolFacts, &str),
) -> Result<(), StageItemFailure> {
    let (modules, symbol, name) = query;
    let parent = target.index.parents.get(&symbol.input.symbol_id);
    let prefix = match parent.and_then(|parent| target.index.qualtype.owners.get(parent)) {
        Some(parent) if parent.kind == SymbolKind::Module => parent.name.as_str(),
        Some(_) => return Ok(()),
        None if parent.is_some() => return Ok(()),
        None => "",
    };
    let Some(name) = super::rust_inline_modules::qualified_name(prefix, name)? else {
        return Ok(());
    };
    if let Some(unique) = modules.get_mut(&name) {
        *unique = false;
        return Ok(());
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(String, bool)>() + name.len()),
    )?;
    modules.try_reserve(1).map_err(|_| StageItemFailure)?;
    modules.insert(name, true);
    Ok(())
}

fn retain_raw_aliases(
    target: &mut ResolutionIndexTarget<'_>,
    query: (&NativeFileFacts, &NativeSymbolFacts),
) -> Result<(), StageItemFailure> {
    let (file, symbol) = query;
    if symbol.kind == SymbolKind::Import || !symbol.input.qualified_name.contains("r#") {
        return Ok(());
    }
    let ordinal = *target
        .index
        .file_ordinals
        .get(&file.file.file_id)
        .ok_or(StageItemFailure)?;
    for (position, original) in [&symbol.name, &symbol.input.qualified_name]
        .into_iter()
        .enumerate()
    {
        if !original.contains("r#")
            || original.len() > 1_024
            || position == 1 && original == &symbol.name
        {
            continue;
        }
        target.budget.charge(usize_to_u64(original.len()))?;
        let normalized = normalized_path(original)?;
        super::push_candidate(
            &mut target.index.rust_paths.declarations.raw_candidates,
            super::ResolutionCandidateInsertion {
                key: &normalized,
                symbol,
                parent_symbol_id: target.index.parents.get(&symbol.input.symbol_id),
                file_ordinal: ordinal,
                language: "rust",
                visibility: symbol.visibility,
            },
            target.budget,
        )?;
    }
    Ok(())
}

pub(super) fn raw_candidates_for_file<'a>(
    index: &'a ResolutionIndex,
    query: (&str, &FileId),
) -> &'a [super::ResolutionCandidate] {
    let Some(ordinal) = index.file_ordinals.get(query.1) else {
        return &[];
    };
    index
        .rust_paths
        .declarations
        .raw_candidates
        .get(query.0)
        .map_or(&[], |bucket| bucket.for_file(*ordinal))
}

fn retain_file(
    budget: &mut super::ResolveBudget,
    query: (&mut HashSet<FileId>, &FileId),
) -> Result<(), StageItemFailure> {
    let (files, file) = query;
    if files.contains(file) {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<FileId>() + file.as_str().len()),
    )?;
    files.try_reserve(1).map_err(|_| StageItemFailure)?;
    files.insert(file.clone());
    Ok(())
}

pub(super) fn finalize<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut roots = HashSet::new();
    for file in &target.index.rust_paths.declarations.standard_files {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(root) = super::rust_root_ownership::root(target.index, file) {
            retain_file(target.budget, (&mut roots, root))?;
        }
    }
    target.index.rust_paths.declarations.standard_roots = roots;
    Ok(())
}

pub(super) fn namespace_overridden(index: &ResolutionIndex, file: &FileId) -> bool {
    let declarations = &index.rust_paths.declarations;
    declarations.standard_files.contains(file)
        || super::rust_root_ownership::root(index, file)
            .is_some_and(|root| declarations.standard_roots.contains(root))
}

pub(super) fn module_declared(index: &ResolutionIndex, query: (&FileId, &str)) -> Option<bool> {
    index
        .rust_paths
        .declarations
        .modules
        .get(query.0)
        .and_then(|names| names.get(query.1))
        .copied()
}

pub(super) fn raw_name(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

pub(super) fn same_path(left: &str, right: &str) -> bool {
    left.split("::")
        .map(raw_name)
        .eq(right.split("::").map(raw_name))
}

pub(super) fn normalized_path(path: &str) -> Result<Cow<'_, str>, StageItemFailure> {
    if !path.contains("r#") {
        return Ok(Cow::Borrowed(path));
    }
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(path.len())
        .map_err(|_| StageItemFailure)?;
    for (position, component) in path.split("::").enumerate() {
        if position > 0 {
            normalized.push_str("::");
        }
        normalized.push_str(raw_name(component));
    }
    Ok(Cow::Owned(normalized))
}

pub(super) fn supported_raw_path(path: &str) -> bool {
    let Some((module, leaf)) = path.rsplit_once("::") else {
        return false;
    };
    matches!(module.split("::").next(), Some("crate" | "self" | "super"))
        && !module.contains("r#")
        && leaf.starts_with("r#")
        && !raw_name(leaf).is_empty()
        && raw_name(leaf)
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "rust" || request.dispatch != ReferenceDispatch::Static {
        return Ok(None);
    }
    if super::qualtype_generics::opaque_header(index, request, cancelled)?
        || super::rust_use_bindings::standard_namespace_fenced(index, request)
        || super::rust_path_resolution::ambiguous_path(index, request, cancelled)?
    {
        return Ok(Some(ReferenceResolution::unresolved(
            super::UNRESOLVED_PROVENANCE,
        )));
    }
    if !request.name.starts_with("::") && !request.name.contains("r#") {
        return Ok(None);
    }
    Ok(Some(
        super::rust_path_resolution::resolve(index, request, cancelled)?
            .unwrap_or_else(|| ReferenceResolution::unresolved(super::UNRESOLVED_PROVENANCE)),
    ))
}
