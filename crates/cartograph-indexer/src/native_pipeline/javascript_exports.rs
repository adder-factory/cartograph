use super::{
    BTreeSet, FileId, HashMap, ImportBindingKind, ImportCandidateFilter, ModuleImportQuery,
    RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndex, ResolveBudget, ResolvedTarget,
    StageItemFailure, SymbolId, import_binding_target, javascript_modules,
    javascript_value_import_usage, resolution_candidates_for_file, resolve_reexport_target,
    select_candidate, size_of, try_clone_text, usize_to_u64,
};

const MAX_BARREL_DEPTH: usize = 8;
const MAX_BARREL_VISITS: usize = 256;
const WILDCARD_IMPORT_PROVENANCE: &str = "native-wildcard-import";

#[derive(Default)]
pub(super) struct ExportIndex {
    by_file: HashMap<FileId, Vec<usize>>,
    locations: HashMap<SymbolId, CandidateLocation>,
    uncertain: BTreeSet<FileId>,
}

pub(super) fn mark_uncertain(
    index: &mut ExportIndex,
    file: &FileId,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !index.uncertain.contains(file) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(file.as_str().len() + size_of::<FileId>()),
        )?;
        index.uncertain.insert(file.clone());
    }
    Ok(())
}

struct CandidateLocation {
    name: String,
    file_id: FileId,
}

pub(super) fn prepare<Cancel>(
    index: &mut ResolutionIndex,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    budget.charge(usize_to_u64(
        MAX_BARREL_DEPTH.saturating_mul(size_of::<&FileId>()),
    ))?;
    index_reexports(index, budget, cancelled)?;
    if index.javascript_exports.by_file.is_empty() {
        return Ok(());
    }
    for name in &index.candidate_order {
        for candidate in index.candidates.get(name).ok_or(StageItemFailure)?.iter() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if !candidate.export.exported
                || index
                    .javascript_exports
                    .locations
                    .contains_key(&candidate.symbol_id)
            {
                continue;
            }
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    + usize_to_u64(
                        name.len()
                            + candidate.symbol_id.as_str().len()
                            + candidate.file_id.as_str().len(),
                    )
                    + usize_to_u64(size_of::<CandidateLocation>()),
            )?;
            index.javascript_exports.locations.insert(
                candidate.symbol_id.clone(),
                CandidateLocation {
                    name: try_clone_text(name)?,
                    file_id: candidate.file_id.clone(),
                },
            );
        }
    }
    Ok(())
}

fn index_reexports<Cancel>(
    index: &mut ResolutionIndex,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for (position, re_export) in index.re_exports.iter().enumerate() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if re_export.namespace {
            continue;
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<usize>() + size_of::<FileId>()),
        )?;
        index
            .javascript_exports
            .by_file
            .entry(re_export.source_file_id.clone())
            .or_default()
            .push(position);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ExportMatch<'a> {
    Absent,
    Unique(&'a SymbolId, bool),
    Ambiguous,
}

#[derive(Clone, Copy)]
struct ExportQuery<'a> {
    index: &'a ResolutionIndex,
    file: &'a FileId,
    name: &'a str,
    include_default: bool,
}

struct ExportWalk<'index, 'cancel, Cancel> {
    stack: Vec<&'index FileId>,
    remaining: usize,
    cancelled: &'cancel mut Cancel,
}

fn named_export<'a, Cancel>(
    query: ExportQuery<'a>,
    walk: &mut ExportWalk<'a, '_, Cancel>,
) -> Result<ExportMatch<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if (walk.cancelled)() {
        return Err(StageItemFailure);
    }
    if walk.remaining == 0 {
        return Ok(ExportMatch::Ambiguous);
    }
    walk.remaining -= 1;
    if let Some(explicit) = query
        .index
        .exports
        .get(query.file)
        .and_then(|exports| exports.get(query.name))
    {
        return Ok(match explicit {
            Some(target) if query.include_default || !target.default_export => {
                ExportMatch::Unique(&target.symbol_id, false)
            }
            Some(_) => ExportMatch::Absent,
            None => ExportMatch::Ambiguous,
        });
    }
    if query
        .index
        .javascript_exports
        .uncertain
        .contains(query.file)
    {
        return Ok(ExportMatch::Ambiguous);
    }
    if walk.stack.contains(&query.file) {
        return Ok(ExportMatch::Absent);
    }
    if walk.stack.len() >= MAX_BARREL_DEPTH {
        return Ok(ExportMatch::Ambiguous);
    }
    walk.stack.push(query.file);
    let result = nested_export(query, walk);
    walk.stack.pop();
    result
}

fn nested_export<'a, Cancel>(
    query: ExportQuery<'a>,
    walk: &mut ExportWalk<'a, '_, Cancel>,
) -> Result<ExportMatch<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(positions) = query.index.javascript_exports.by_file.get(query.file) else {
        return Ok(ExportMatch::Absent);
    };
    let mut found = ExportMatch::Absent;
    for position in positions {
        if (walk.cancelled)() {
            return Err(StageItemFailure);
        }
        let re_export = query
            .index
            .re_exports
            .get(*position)
            .ok_or(StageItemFailure)?;
        let Some(file) = resolve_reexport_target(query.index, re_export) else {
            return Ok(ExportMatch::Ambiguous);
        };
        let nested = named_export(
            ExportQuery {
                file,
                include_default: false,
                ..query
            },
            walk,
        )?;
        let nested = lower_reexport(query.index, re_export, nested);
        found = merge_export(found, nested);
        if matches!(found, ExportMatch::Ambiguous) {
            break;
        }
    }
    Ok(found)
}

fn merge_export<'a>(left: ExportMatch<'a>, right: ExportMatch<'a>) -> ExportMatch<'a> {
    match (left, right) {
        (ExportMatch::Absent, hit) | (hit, ExportMatch::Absent) => hit,
        (ExportMatch::Unique(left, left_fallback), ExportMatch::Unique(right, right_fallback))
            if left == right =>
        {
            ExportMatch::Unique(left, left_fallback || right_fallback)
        }
        _ => ExportMatch::Ambiguous,
    }
}

fn lower_reexport<'a>(
    index: &ResolutionIndex,
    re_export: &super::ProjectReExport,
    matched: ExportMatch<'a>,
) -> ExportMatch<'a> {
    let ExportMatch::Unique(symbol, previous) = matched else {
        return matched;
    };
    let Some(source) = index.modules.files.get(&re_export.source_file_id) else {
        return ExportMatch::Ambiguous;
    };
    let request = super::ModuleResolutionRequest {
        importing_path: &source.path,
        specifier: &re_export.module_specifier,
        importing_language: &source.language,
    };
    let fallback = javascript_modules::fallback_provenance(&index.modules, request).is_some();
    ExportMatch::Unique(symbol, previous || fallback)
}

fn export_match<'a, Cancel>(
    query: ExportQuery<'a>,
    cancelled: &mut Cancel,
) -> Result<ExportMatch<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut walk = ExportWalk {
        stack: Vec::with_capacity(MAX_BARREL_DEPTH),
        remaining: MAX_BARREL_VISITS,
        cancelled,
    };
    named_export(query, &mut walk)
}

#[derive(Clone, Copy)]
pub(super) struct ExportEdgeQuery<'a> {
    pub(super) index: &'a ResolutionIndex,
    pub(super) file: &'a FileId,
    pub(super) name: &'a str,
    pub(super) target: &'a SymbolId,
    pub(super) namespace: Option<&'a super::ProjectReExport>,
}

pub(super) fn edge_evidence<Cancel>(
    query: ExportEdgeQuery<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<(f32, &'static str)>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let namespace = query.namespace.is_some();
    let exact = if namespace {
        super::RE_EXPORT_NAMESPACE_PROVENANCE
    } else {
        super::RE_EXPORT_ALL_PROVENANCE
    };
    let source = query
        .index
        .modules
        .files
        .get(query.file)
        .ok_or(StageItemFailure)?;
    if !javascript_modules::module_language(&source.language) {
        return Ok(Some((super::IMPORT_BINDING_CONFIDENCE, exact)));
    }
    let matched = export_match(
        ExportQuery {
            index: query.index,
            file: query.file,
            name: query.name,
            include_default: namespace,
        },
        cancelled,
    )?;
    let matched = if let Some(outer) = query.namespace {
        lower_reexport(query.index, outer, matched)
    } else {
        matched
    };
    let ExportMatch::Unique(target, fallback) = matched else {
        return Ok(None);
    };
    if target != query.target {
        return Ok(None);
    }
    let evidence = if fallback {
        let provenance = if namespace {
            "native-reexport-namespace-fallback"
        } else {
            "native-reexport-all-fallback"
        };
        (super::FRAMEWORK_CONVENTION_CONFIDENCE, provenance)
    } else {
        (super::IMPORT_BINDING_CONFIDENCE, exact)
    };
    Ok(Some(evidence))
}

pub(super) fn resolve_import<Cancel>(
    query: ModuleImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_modules::module_language(query.import.reference.language)
        || query.binding.kind == ImportBindingKind::Default
        || query.imported_name == "default"
        || !query
            .index
            .javascript_exports
            .by_file
            .contains_key(query.module_file_id)
    {
        return Ok(None);
    }
    let matched = export_match(
        ExportQuery {
            index: query.index,
            file: query.module_file_id,
            name: query.imported_name,
            include_default: false,
        },
        cancelled,
    )?;
    let ExportMatch::Unique(symbol, fallback) = matched else {
        return Ok(None);
    };
    let Some(location) = query.index.javascript_exports.locations.get(symbol) else {
        return Ok(None);
    };
    let filter = ImportCandidateFilter {
        index: query.index,
        reference: query.import.reference,
        imported_name: query.imported_name,
        module_file_id: &location.file_id,
        javascript_value_usage: javascript_value_import_usage(query.import),
    };
    let candidate = select_candidate(
        resolution_candidates_for_file(query.index, &location.name, &location.file_id),
        |candidate| {
            &candidate.symbol_id == symbol
                && !matches!(
                    candidate.kind,
                    super::SymbolKind::Export | super::SymbolKind::Import
                )
                && filter.matches(candidate)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| ResolvedTarget {
        confidence: if fallback {
            super::FRAMEWORK_CONVENTION_CONFIDENCE
        } else {
            super::IMPORT_BINDING_CONFIDENCE
        },
        provenance: if fallback {
            "native-wildcard-import-fallback"
        } else {
            WILDCARD_IMPORT_PROVENANCE
        },
        ..import_binding_target(candidate)
    }))
}
