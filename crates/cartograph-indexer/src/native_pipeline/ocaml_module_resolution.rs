//! An exact subset: unambiguous implementation units, root opens, plain modules.
mod open_index;

use super::{
    FileId, HashMap, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind,
    ReferenceResolution, ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolKind,
    resolution_candidates_for_file, resolve_lexical, select_candidate, size_of, try_clone_text,
    usize_to_u64,
};
use std::collections::HashSet;

const CALL_METADATA: &str = "<ocaml-call>";
const OPEN_METADATA: &str = "<ocaml-open>";
const FENCE_METADATA: &str = "<ocaml-fence>";
const SHADOW_METADATA: &str = "<ocaml-shadow>";
const VALUE_METADATA: &str = "<ocaml-value-fence>";
const MODULE_PROVENANCE: &str = "native-ocaml-module";
const MODULE_CONFIDENCE: f32 = 1.0;

#[derive(Default)]
pub(super) struct ModuleIndex {
    units: HashMap<String, Option<FileId>>,
    files: HashMap<FileId, FileEvidence>,
}

#[derive(Default)]
struct FileEvidence {
    calls: HashMap<(u64, u64), String>,
    opens: Vec<(String, u64)>,
    fences: HashSet<String>,
    shadows: HashSet<String>,
    value_fences: HashSet<String>,
    opened: open_index::OpenIndex,
}

pub(super) fn metadata_binding(language: &str, binding: &super::ExtractedImportBinding) -> bool {
    language == "ocaml"
        && matches!(
            binding.module_specifier.as_str(),
            CALL_METADATA | OPEN_METADATA | FENCE_METADATA | SHADOW_METADATA | VALUE_METADATA
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
    if !matches!(file.file.language.as_str(), "ocaml" | "ocaml_interface") {
        return Ok(());
    }
    index_unit(target, file)?;
    let mut evidence = FileEvidence::default();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !metadata_binding(&file.file.language, binding) {
            continue;
        }
        record(&mut evidence, (binding, target.budget))?;
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(size_of::<(FileId, FileEvidence)>() + file.file.file_id.as_str().len()),
    )?;
    target
        .index
        .ocaml_modules
        .files
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .ocaml_modules
        .files
        .insert(file.file.file_id.clone(), evidence);
    Ok(())
}

fn record(
    evidence: &mut FileEvidence,
    entry: (&super::ExtractedImportBinding, &mut super::ResolveBudget),
) -> Result<(), StageItemFailure> {
    let (binding, budget) = entry;
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(size_of::<((u64, u64), String)>() + binding.imported_name.len()),
    )?;
    match binding.module_specifier.as_str() {
        CALL_METADATA => record_call(evidence, binding)?,
        OPEN_METADATA => {
            evidence
                .opens
                .try_reserve(1)
                .map_err(|_| StageItemFailure)?;
            evidence.opens.push((
                try_clone_text(&binding.imported_name)?,
                binding.span.end_byte(),
            ));
        }
        metadata => {
            let names = match metadata {
                SHADOW_METADATA => &mut evidence.shadows,
                VALUE_METADATA => &mut evidence.value_fences,
                _ => &mut evidence.fences,
            };
            names.try_reserve(1).map_err(|_| StageItemFailure)?;
            names.insert(try_clone_text(&binding.imported_name)?);
        }
    }
    Ok(())
}

fn record_call(
    evidence: &mut FileEvidence,
    binding: &super::ExtractedImportBinding,
) -> Result<(), StageItemFailure> {
    let key = (binding.span.start_byte(), binding.span.end_byte());
    if let Some(existing) = evidence.calls.get_mut(&key) {
        if *existing != binding.imported_name {
            existing.clear();
        }
        return Ok(());
    }
    evidence
        .calls
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    evidence
        .calls
        .insert(key, try_clone_text(&binding.imported_name)?);
    Ok(())
}

fn index_unit(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
) -> Result<(), StageItemFailure> {
    let filename = file
        .file
        .normalized_path
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let Some((stem, extension)) = filename.rsplit_once('.') else {
        return Ok(());
    };
    if !matches!(extension, "ml" | "mli")
        || !stem.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
    {
        return Ok(());
    }
    let unit = format!("{}{}", stem[..1].to_ascii_uppercase(), &stem[1..]);
    let units = &mut target.index.ocaml_modules.units;
    if let Some(existing) = units.get_mut(&unit) {
        *existing = None;
        return Ok(());
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(String, Option<FileId>)>()
                    + unit.len()
                    + file.file.file_id.as_str().len(),
            ),
    )?;
    units.try_reserve(1).map_err(|_| StageItemFailure)?;
    units.insert(unit, (extension == "ml").then(|| file.file.file_id.clone()));
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
    if request.language != "ocaml" || request.kind != ReferenceKind::Calls {
        return Ok(None);
    }
    if cancelled() {
        return Err(StageItemFailure);
    }
    let Some(evidence) = index.ocaml_modules.files.get(request.file_id) else {
        return Ok(None);
    };
    if evidence.fences.contains("*") || !root_caller(index, request) {
        return Ok(None);
    }
    let Some(name) = evidence
        .calls
        .get(&(request.span.start_byte(), request.span.end_byte()))
    else {
        return Ok(None);
    };
    if name.contains('.') {
        let root = name.split('.').next().unwrap_or_default();
        if evidence.opened.shadowed(root, request.span.start_byte()) {
            return Ok(None);
        }
        return qualified(index, (request, evidence, name), cancelled).map(resolution);
    }
    if value_fenced(evidence, name)
        || evidence.shadows.contains(request.name)
        || resolve_lexical(index, request, cancelled)?.is_some()
    {
        return Ok(None);
    }
    Ok(evidence
        .opened
        .function(name, request.span.start_byte())
        .map(|symbol_id| {
            ReferenceResolution::resolved(ResolvedTarget {
                symbol_id: symbol_id.clone(),
                kind: SymbolKind::Function,
                confidence: MODULE_CONFIDENCE,
                provenance: MODULE_PROVENANCE,
            })
        }))
}

fn value_fenced(evidence: &FileEvidence, name: &str) -> bool {
    evidence.value_fences.contains("*")
        || evidence
            .value_fences
            .contains(name.rsplit('.').next().unwrap_or_default())
}

fn root_caller(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    request.owner.is_none_or(|owner| {
        index
            .parents
            .get(owner)
            .is_none_or(|parent| index.file_symbols.get(request.file_id) == Some(parent))
    })
}

pub(super) fn finalize<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if target.index.ocaml_modules.files.is_empty() {
        return Ok(());
    }
    let namespaces = open_index::Namespaces::build(target.index, target.budget, cancelled)?;
    let mut prepared = Vec::new();
    for (file_id, evidence) in &target.index.ocaml_modules.files {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let opened = open_index::OpenIndex::prepare(
            target.index,
            (file_id, evidence),
            (&namespaces, target.budget, cancelled),
        )?;
        target.budget.charge(usize_to_u64(
            size_of::<(FileId, open_index::OpenIndex)>() + file_id.as_str().len(),
        ))?;
        prepared.try_reserve(1).map_err(|_| StageItemFailure)?;
        prepared.push((file_id.clone(), opened));
    }
    for (file_id, opened) in prepared {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let evidence = target
            .index
            .ocaml_modules
            .files
            .get_mut(&file_id)
            .ok_or(StageItemFailure)?;
        evidence.opened = opened;
        evidence.opens.clear();
    }
    Ok(())
}

fn function_member(
    candidate: &ResolutionCandidate,
    query: (&ResolutionRequest<'_>, &FileId, &str),
) -> bool {
    candidate.kind == SymbolKind::Function
        && candidate.qualified_name == query.2
        && (query.1 != query.0.file_id || candidate.declaration_span.0 <= query.0.span.start_byte())
}

fn qualified<'index, Cancel>(
    index: &'index ResolutionIndex,
    query: (&ResolutionRequest<'_>, &FileEvidence, &str),
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, evidence, name) = query;
    let Some((root, _)) = name.split_once('.') else {
        return Ok(None);
    };
    if evidence.fences.contains(root) {
        return Ok(None);
    }
    let Some((file, name)) = scope(
        index,
        (request.file_id, name, request.span.start_byte()),
        cancelled,
    )?
    else {
        return Ok(None);
    };
    if index
        .ocaml_modules
        .files
        .get(file)
        .is_some_and(|target| value_fenced(target, name))
    {
        return Ok(None);
    }
    select_candidate(
        resolution_candidates_for_file(index, name, file),
        |candidate| function_member(candidate, (request, file, name)),
        cancelled,
    )
}

fn scope<'index, 'name, Cancel>(
    index: &'index ResolutionIndex,
    query: (&'index FileId, &'name str, u64),
    cancelled: &mut Cancel,
) -> Result<Option<(&'index FileId, &'name str)>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file_id, name, position) = query;
    let (root, tail) = name.split_once('.').unwrap_or((name, ""));
    let mut modules = 0_usize;
    for candidate in resolution_candidates_for_file(index, root, file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind == SymbolKind::Module
            && candidate.qualified_name == root
            && candidate.top_level
            && candidate.declaration_span.0 <= position
        {
            modules += 1;
        }
    }
    let result = match modules {
        0 => index
            .ocaml_modules
            .units
            .get(root)
            .and_then(Option::as_ref)
            .map(|file| (file, tail)),
        1 => Some((file_id, name)),
        _ => None,
    };
    Ok(result.filter(|(file, _)| {
        index
            .ocaml_modules
            .files
            .get(*file)
            .is_some_and(|evidence| {
                !evidence.fences.contains("*")
                    && !name.split('.').any(|part| evidence.fences.contains(part))
            })
    }))
}

fn resolution(candidate: Option<&ResolutionCandidate>) -> Option<ReferenceResolution> {
    candidate.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: MODULE_CONFIDENCE,
            provenance: MODULE_PROVENANCE,
        })
    })
}
