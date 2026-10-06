use super::{
    FileId, FileInput, HashMap, ImportBindingKind, ModuleFileMatch, NormalizedPath,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind, ResolutionIndex, ResolutionRequest,
    ResolveBudget, ResolvedTarget, SourceLanguage, StageItemFailure, SymbolKind, module_file_match,
    size_of, try_clone_text, usize_to_u64,
};

#[derive(Default)]
pub(super) struct SuffixIndex {
    by_family: HashMap<&'static str, HashMap<String, Option<FileId>>>,
}

pub(super) fn index_path<Cancel>(
    paths: &mut SuffixIndex,
    file: &FileInput,
    (budget, cancelled): (&mut ResolveBudget, &mut Cancel),
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(family) = path_family(&file.language) else {
        return Ok(());
    };
    for (position, _) in file.normalized_path.match_indices('/') {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let suffix = &file.normalized_path[position + 1..];
        if suffix.contains('/') {
            insert_suffix(paths, (family, suffix, &file.file_id), budget)?;
        }
    }
    Ok(())
}

fn insert_suffix(
    paths: &mut SuffixIndex,
    (family, suffix, file): (&'static str, &str, &FileId),
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !paths.by_family.contains_key(family) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<HashMap<String, Option<FileId>>>()),
        )?;
        paths
            .by_family
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
    }
    let entries = paths.by_family.entry(family).or_default();
    if let Some(existing) = entries.get_mut(suffix) {
        if existing.as_ref() != Some(file) {
            *existing = None;
        }
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(String, Option<FileId>)>() + suffix.len() + file.as_str().len(),
            ),
    )?;
    entries.try_reserve(1).map_err(|_| StageItemFailure)?;
    entries.insert(try_clone_text(suffix)?, Some(file.clone()));
    Ok(())
}

fn path_family(language: &str) -> Option<&'static str> {
    match language {
        "liquid" => Some("liquid"),
        "c" | "cpp" | "cuda" => Some("native-c"),
        "glsl" => Some("glsl"),
        "hlsl" => Some("hlsl"),
        _ => None,
    }
}

pub(super) fn resolve_reference<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::Imports || !eligible_path_reference(request, cancelled)? {
        return Ok(None);
    }
    let name = request.name.trim_start_matches("./");
    if !name.contains('/') || NormalizedPath::parse(name).is_err() {
        return Ok(None);
    }
    match module_file_match(
        index.modules.exact.get(name),
        &index.modules.files,
        request.language,
    ) {
        ModuleFileMatch::Unique(file) => {
            return file_target(index, file, (0.95, "native-file-path-exact"));
        }
        ModuleFileMatch::Ambiguous => return Ok(None),
        ModuleFileMatch::Missing => {}
    }
    let Some(family) = path_family(request.language) else {
        return Ok(None);
    };
    let candidate = index
        .modules
        .file_paths
        .by_family
        .get(family)
        .and_then(|entries| entries.get(name))
        .and_then(Option::as_ref);
    match candidate {
        Some(file) => file_target(index, file, (0.85, "native-file-path-suffix")),
        None => Ok(None),
    }
}

fn eligible_path_reference<Cancel>(
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language == SourceLanguage::Liquid.as_str() {
        return Ok(true);
    }
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.span == request.span
            && binding.module_specifier == request.name
            && binding.kind == ImportBindingKind::IncludeQuoted
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn file_target(
    index: &ResolutionIndex,
    file: &FileId,
    (confidence, provenance): (f32, &'static str),
) -> Result<Option<ResolvedTarget>, StageItemFailure> {
    let symbol = index.file_symbols.get(file).ok_or(StageItemFailure)?;
    Ok(Some(ResolvedTarget {
        symbol_id: symbol.clone(),
        kind: SymbolKind::File,
        confidence,
        provenance,
    }))
}
