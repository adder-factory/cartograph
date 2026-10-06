//! Explicit Elixir module/alias calls; unsupported forms leave base lookup intact.
//! No project-wide simple-name or receiver-inference fallback participates.
use super::{
    FileId, HashMap, ImportBindingKind, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceResolution, ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolKind, reference_kind_candidate,
    resolution_candidates_for_file, select_candidate, size_of, usize_to_u64,
};

#[derive(Default)]
pub(super) struct ModuleCallIndex {
    calls: HashMap<FileId, HashMap<(u64, u64), String>>,
    aliases: HashMap<FileId, HashMap<String, Option<StaticAlias>>>,
    fences: HashMap<FileId, Vec<(u64, u64)>>,
    public_members: HashMap<FileId, std::collections::HashSet<String>>,
}

struct StaticAlias {
    module_specifier: String,
    local_name: String,
    span: super::SourceSpan,
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "elixir" {
        return Ok(());
    }
    index_aliases(target, file, cancelled)?;
    index_members(target, file, cancelled)?;
    let mut calls = HashMap::<(u64, u64), String>::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !callee_binding(&file.file.language, binding) {
            continue;
        }
        let key = (binding.span.start_byte(), binding.span.end_byte());
        let name = format!("{}.{}", binding.module_specifier, binding.imported_name);
        if let Some(existing) = calls.get_mut(&key) {
            if *existing != name {
                existing.clear();
            }
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<((u64, u64), String)>()))
                .saturating_add(usize_to_u64(name.len())),
        )?;
        calls.try_reserve(1).map_err(|_| StageItemFailure)?;
        calls.insert(key, name);
    }
    if !calls.is_empty() {
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(
                    size_of::<(FileId, HashMap<(u64, u64), String>)>(),
                ))
                .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
        )?;
        target
            .index
            .module_calls
            .calls
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        target
            .index
            .module_calls
            .calls
            .insert(file.file.file_id.clone(), calls);
    }
    Ok(())
}

pub(super) fn callee_binding(language: &str, binding: &super::ExtractedImportBinding) -> bool {
    language == "elixir"
        && binding.kind == ImportBindingKind::Named
        && binding.local_name == binding.imported_name
}

pub(super) fn metadata_binding(language: &str, binding: &super::ExtractedImportBinding) -> bool {
    callee_binding(language, binding)
        || language == "elixir"
            && binding.kind == ImportBindingKind::Namespace
            && binding.imported_name == "*"
}

fn index_aliases<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut aliases = HashMap::new();
    let mut fences = Vec::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.kind != ImportBindingKind::Namespace
            || binding.module_specifier == "<elixir-public-function>"
        {
            continue;
        }
        if binding.module_specifier == "<elixir-alias-fence>" {
            target.budget.charge(16)?;
            fences.try_reserve(1).map_err(|_| StageItemFailure)?;
            fences.push((binding.span.start_byte(), binding.span.end_byte()));
            continue;
        }
        if let Some(existing) = aliases.get_mut(&binding.local_name) {
            *existing = None;
            continue;
        }
        target
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(
                size_of::<StaticAlias>()
                    + binding.local_name.len() * 2
                    + binding.module_specifier.len()
                    + binding.imported_name.len(),
            )))?;
        aliases.try_reserve(1).map_err(|_| StageItemFailure)?;
        aliases.insert(
            super::try_clone_text(&binding.local_name)?,
            Some(StaticAlias {
                module_specifier: super::try_clone_text(&binding.module_specifier)?,
                local_name: super::try_clone_text(&binding.local_name)?,
                span: binding.span,
            }),
        );
    }
    fences.sort_unstable();
    let mut maximum = 0;
    for (_, end) in &mut fences {
        if cancelled() {
            return Err(StageItemFailure);
        }
        maximum = maximum.max(*end);
        *end = maximum;
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE * 2
            + usize_to_u64(
                size_of::<(FileId, Vec<(u64, u64)>)>()
                    + size_of::<(FileId, HashMap<String, Option<StaticAlias>>)>()
                    + file.file.file_id.as_str().len() * 2,
            ),
    )?;
    target
        .index
        .module_calls
        .aliases
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .module_calls
        .fences
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .module_calls
        .aliases
        .insert(file.file.file_id.clone(), aliases);
    target
        .index
        .module_calls
        .fences
        .insert(file.file.file_id.clone(), fences);
    Ok(())
}

fn index_members<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut names = std::collections::HashSet::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.module_specifier != "<elixir-public-function>" {
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<String>() + binding.local_name.len()),
        )?;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(super::try_clone_text(&binding.local_name)?);
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(FileId, std::collections::HashSet<String>)>()
                    + file.file.file_id.as_str().len(),
            ),
    )?;
    target
        .index
        .module_calls
        .public_members
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target
        .index
        .module_calls
        .public_members
        .insert(file.file.file_id.clone(), names);
    Ok(())
}

fn lookup_name<'a>(index: &'a ResolutionIndex, request: &ResolutionRequest<'_>) -> Option<&'a str> {
    if request.kind != super::ReferenceKind::Calls {
        return None;
    }
    index
        .module_calls
        .calls
        .get(request.file_id)?
        .get(&(request.span.start_byte(), request.span.end_byte()))
        .filter(|name| !name.is_empty())
        .map(String::as_str)
}

pub(super) fn qualified<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.language != "elixir" {
        return Ok(None);
    }
    let name = lookup_name(index, request).unwrap_or(request.name);
    if !name.contains('.') {
        return Ok(None);
    }
    elixir(index, &ResolutionRequest { name, ..*request }, cancelled)
}

fn elixir<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some((module, member)) = request.name.rsplit_once('.') else {
        return Ok(None);
    };
    if fenced(index, request) {
        return Ok(None);
    }
    let root = module.split('.').next().unwrap_or(module);
    let mut alias = None;
    if let Some(binding) = index
        .module_calls
        .aliases
        .get(request.file_id)
        .and_then(|aliases| aliases.get(root))
    {
        let Some(binding) = binding.as_ref() else {
            return Ok(None);
        };
        if let Some(suffix) = alias_suffix(binding, (request, module)) {
            alias = Some(format!("{}{suffix}", binding.module_specifier));
        }
    }
    let module = alias.as_deref().unwrap_or(module);
    if nested_alias(index, (request, module), cancelled)? {
        return Ok(None);
    }
    let Some(modules) = index.candidates.get(module) else {
        return Ok(None);
    };
    let owner = select_candidate(
        modules.as_slice(),
        |candidate| {
            candidate.kind == SymbolKind::Module
                && candidate.qualified_name == module
                && index
                    .modules
                    .files
                    .get(&candidate.file_id)
                    .is_some_and(|file| file.language == "elixir")
        },
        cancelled,
    )?;
    let Some(owner) = owner else {
        return Ok(None);
    };
    let qualified = format!("{module}.{member}");
    let candidate = owned_member(index, (request, owner, &qualified), cancelled)?;
    Ok(resolution(candidate, "native-elixir-module"))
}

fn fenced(index: &ResolutionIndex, request: &ResolutionRequest<'_>) -> bool {
    let Some(fences) = index.module_calls.fences.get(request.file_id) else {
        return false;
    };
    let position = fences.partition_point(|(start, _)| *start <= request.span.start_byte());
    position
        .checked_sub(1)
        .and_then(|position| fences.get(position))
        .is_some_and(|(_, end)| *end >= request.span.end_byte())
}

fn nested_alias<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, name) = query;
    let root = name.split('.').next().unwrap_or(name);
    for candidate in resolution_candidates_for_file(index, root, request.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind == SymbolKind::Module && candidate.qualified_name != root {
            return Ok(true);
        }
    }
    Ok(false)
}

fn owned_member<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (&ResolutionRequest<'_>, &ResolutionCandidate, &str),
    cancelled: &mut Cancel,
) -> Result<Option<&'a ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, owner, name) = query;
    select_candidate(
        resolution_candidates_for_file(index, name, &owner.file_id),
        |candidate| {
            candidate.qualified_name == name
                && candidate.parent_symbol_id.as_ref() == Some(&owner.symbol_id)
                && reference_kind_candidate(request.kind, candidate)
        },
        cancelled,
    )
    .map(|candidate| {
        candidate.filter(|_| {
            index
                .module_calls
                .public_members
                .get(&owner.file_id)
                .is_some_and(|names| names.contains(name))
        })
    })
}

fn resolution(
    candidate: Option<&ResolutionCandidate>,
    provenance: &'static str,
) -> Option<ReferenceResolution> {
    candidate.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: 1.0,
            provenance,
        })
    })
}

fn binding_contains(binding: &StaticAlias, request: &ResolutionRequest<'_>) -> bool {
    binding.span.start_byte() <= request.span.start_byte()
        && binding.span.end_byte() >= request.span.end_byte()
}

fn alias_suffix<'a>(
    binding: &StaticAlias,
    query: (&ResolutionRequest<'_>, &'a str),
) -> Option<&'a str> {
    let (request, module) = query;
    if !binding_contains(binding, request) {
        return None;
    }
    module
        .strip_prefix(&binding.local_name)
        .filter(|suffix| suffix.is_empty() || suffix.starts_with('.'))
}
