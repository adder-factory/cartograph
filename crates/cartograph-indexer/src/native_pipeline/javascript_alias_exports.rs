//! Named export landmarks are followed through their explicit module bindings.

use super::{
    FileId, HashMap, ImportBindingKind, ImportCandidateFilter, ModuleImportQuery,
    ModuleResolutionRequest, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ResolveBudget,
    ResolvedTarget, StageItemFailure, SymbolKind, import_binding_target, javascript_family_name,
    javascript_modules, javascript_value_import_usage, resolution_candidates_for_file,
    resolve_module_file, select_candidate, size_of, try_clone_text, usize_to_u64,
};
use std::collections::HashSet;

mod syntax;

const MAX_ALIAS_HOPS: usize = 16;
const PROVENANCE: &str = "native-reexport-alias";
const FALLBACK_PROVENANCE: &str = "native-reexport-alias-fallback";

#[derive(Default)]
pub(super) struct AliasIndex {
    files: HashMap<FileId, HashMap<String, Alias>>,
    runtime: HashSet<super::SymbolId>,
    type_only: HashSet<super::SymbolId>,
}

pub(super) use syntax::index_file as index_syntax;

enum Alias {
    Bound { module: String, imported: String },
    Unproven,
    TypeOnly,
    Ambiguous,
}

enum AliasResolution {
    Resolved(ResolvedTarget),
    Rejected,
    Unproven,
}

#[derive(Clone, Copy)]
struct AliasEntry<'facts> {
    file: &'facts FileId,
    name: &'facts str,
    binding: &'facts super::ExtractedImportBinding,
    owner: &'facts super::SymbolId,
}

pub(super) fn index_file<Cancel>(
    index: &mut AliasIndex,
    file: &NativeFileFacts,
    (budget, cancelled): (&mut ResolveBudget, &mut Cancel),
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_family_name(&file.file.language) {
        return Ok(());
    }
    let names = export_names(file, (budget, cancelled))?;
    let bindings = binding_sites(file, (budget, cancelled))?;
    for reference in &file.references {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if reference.kind != super::ReferenceKind::Exports {
            continue;
        }
        let Some(owner) = reference.owner.as_ref() else {
            continue;
        };
        let Some(name) = names.get(owner) else {
            continue;
        };
        let Some(Some(binding)) =
            bindings.get(&(reference.span.start_byte(), reference.span.end_byte()))
        else {
            continue;
        };
        insert_alias(
            index,
            AliasEntry {
                file: &file.file.file_id,
                name,
                binding,
                owner,
            },
            budget,
        )?;
    }
    Ok(())
}

fn insert_alias(
    index: &mut AliasIndex,
    entry: AliasEntry<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    budget.charge(alias_bytes(entry))?;
    let alias = alias_value(index, entry)?;
    index.files.try_reserve(1).map_err(|_| StageItemFailure)?;
    let aliases = index.files.entry(entry.file.clone()).or_default();
    if let Some(previous) = aliases.get_mut(entry.name) {
        *previous = Alias::Ambiguous;
        return Ok(());
    }
    aliases.try_reserve(1).map_err(|_| StageItemFailure)?;
    aliases.insert(try_clone_text(entry.name)?, alias);
    Ok(())
}

fn alias_bytes(entry: AliasEntry<'_>) -> u64 {
    RESOLUTION_MAP_NODE_ALLOWANCE
        + usize_to_u64(
            size_of::<(FileId, HashMap<String, Alias>)>()
                + size_of::<(String, Alias)>()
                + entry.file.as_str().len()
                + entry.name.len()
                + entry.binding.module_specifier.len()
                + entry.binding.imported_name.len(),
        )
}

fn alias_value(index: &AliasIndex, entry: AliasEntry<'_>) -> Result<Alias, StageItemFailure> {
    Ok(if index.runtime.contains(entry.owner) {
        Alias::Bound {
            module: try_clone_text(&entry.binding.module_specifier)?,
            imported: try_clone_text(&entry.binding.imported_name)?,
        }
    } else if index.type_only.contains(entry.owner) {
        Alias::TypeOnly
    } else {
        Alias::Unproven
    })
}

pub(super) fn is_alias(query: ModuleImportQuery<'_, '_>) -> bool {
    query
        .index
        .javascript_aliases
        .files
        .get(query.module_file_id)
        .is_some_and(|aliases| aliases.contains_key(query.imported_name))
}

fn export_names<'a, Cancel>(
    file: &'a NativeFileFacts,
    (budget, cancelled): (&mut ResolveBudget, &mut Cancel),
) -> Result<HashMap<&'a super::SymbolId, &'a str>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut names = HashMap::new();
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Export {
            continue;
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(&super::SymbolId, &str)>()),
        )?;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(&symbol.input.symbol_id, symbol.name.as_str());
    }
    Ok(names)
}

fn binding_sites<'a, Cancel>(
    file: &'a NativeFileFacts,
    (budget, cancelled): (&mut ResolveBudget, &mut Cancel),
) -> Result<HashMap<(u64, u64), Option<&'a super::ExtractedImportBinding>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut sites = HashMap::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind != ImportBindingKind::Named {
            continue;
        }
        let span = (binding.span.start_byte(), binding.span.end_byte());
        if let Some(previous) = sites.get_mut(&span) {
            *previous = None;
            continue;
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(
                    (u64, u64),
                    Option<&super::ExtractedImportBinding>,
                )>()),
        )?;
        sites.try_reserve(1).map_err(|_| StageItemFailure)?;
        sites.insert(span, Some(binding));
    }
    Ok(sites)
}

pub(super) fn resolve<Cancel>(
    query: ModuleImportQuery<'_, '_>,
    base: Option<ResolvedTarget>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    Ok(match follow(query, cancelled)? {
        AliasResolution::Resolved(target) => Some(target),
        AliasResolution::Rejected => None,
        AliasResolution::Unproven => base,
    })
}

fn follow<Cancel>(
    query: ModuleImportQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<AliasResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut file = query.module_file_id;
    let mut name = query.imported_name;
    let mut fallback = false;
    let mut visited = [None; MAX_ALIAS_HOPS];
    for hop in 0..MAX_ALIAS_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if visited[..hop].contains(&Some((file, name))) {
            return Ok(AliasResolution::Rejected);
        }
        visited[hop] = Some((file, name));
        let Some(alias) = query
            .index
            .javascript_aliases
            .files
            .get(file)
            .and_then(|aliases| aliases.get(name))
        else {
            return declaration(query, (file, name, fallback), cancelled);
        };
        let (module, imported) = match alias {
            Alias::Bound { module, imported } => (module, imported),
            Alias::Unproven => return Ok(AliasResolution::Unproven),
            Alias::TypeOnly if !javascript_value_import_usage(query.import) => {
                return Ok(AliasResolution::Unproven);
            }
            Alias::TypeOnly | Alias::Ambiguous => return Ok(AliasResolution::Rejected),
        };
        let source = query
            .index
            .modules
            .files
            .get(file)
            .ok_or(StageItemFailure)?;
        let request = ModuleResolutionRequest {
            importing_path: &source.path,
            importing_language: &source.language,
            specifier: module,
        };
        fallback |=
            javascript_modules::fallback_provenance(&query.index.modules, request).is_some();
        let Some(target) = resolve_module_file(&query.index.modules, request) else {
            return Ok(AliasResolution::Rejected);
        };
        file = target;
        name = imported;
    }
    Ok(AliasResolution::Unproven)
}

fn declaration<Cancel>(
    query: ModuleImportQuery<'_, '_>,
    (file, name, fallback): (&FileId, &str, bool),
    cancelled: &mut Cancel,
) -> Result<AliasResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let candidates = if name == "default" {
        query
            .index
            .default_exports
            .get(file)
            .map_or(&[][..], Vec::as_slice)
    } else {
        resolution_candidates_for_file(query.index, name, file)
    };
    if unsupported_landmark(candidates, cancelled)? {
        return Ok(AliasResolution::Unproven);
    }
    let filter = ImportCandidateFilter {
        index: query.index,
        reference: query.import.reference,
        imported_name: name,
        module_file_id: file,
        javascript_value_usage: javascript_value_import_usage(query.import),
    };
    let candidate = select_candidate(
        candidates,
        |candidate| {
            !matches!(candidate.kind, SymbolKind::Export | SymbolKind::Import)
                && filter.matches(candidate)
        },
        cancelled,
    )?;
    Ok(candidate.map_or(AliasResolution::Rejected, |candidate| {
        AliasResolution::Resolved(ResolvedTarget {
            confidence: if fallback {
                super::FRAMEWORK_CONVENTION_CONFIDENCE
            } else {
                super::IMPORT_BINDING_CONFIDENCE
            },
            provenance: if fallback {
                FALLBACK_PROVENANCE
            } else {
                PROVENANCE
            },
            ..import_binding_target(candidate)
        })
    }))
}

fn unsupported_landmark<Cancel>(
    candidates: &[super::ResolutionCandidate],
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for candidate in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if matches!(candidate.kind, SymbolKind::Export | SymbolKind::Import) {
            return Ok(true);
        }
    }
    Ok(false)
}
