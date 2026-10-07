//! Bounded C# nominal lookup through existing qualified-name index keys.

use std::collections::HashSet;

use super::{
    ExtractedImportBinding, FileId, HashMap, ImportBindingKind, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceResolution, ResolutionCandidate, ResolutionIndex,
    ResolutionIndexContext, ResolutionIndexTarget, ResolutionRequest, StageItemFailure, SymbolKind,
    Visibility,
    qualtype_resolution::{Selection, nominal, nominal_candidate},
    reference_kind_candidate, size_of, try_clone_text, usize_to_u64,
};

pub(super) const PROVENANCE: &str = "native-csharp-namespace-type";

#[derive(Default)]
pub(super) struct NamespaceImports {
    by_file: HashMap<FileId, NamespaceUsings>,
    root_types: HashSet<String>,
    global_sites: HashMap<FileId, HashSet<u64>>,
}

struct NamespaceUsings {
    namespaces: HashSet<String>,
    namespace_start: u64,
    scoped: bool,
}

#[derive(Clone, Copy)]
struct TypeName<'a> {
    namespace: Option<&'a str>,
    name: &'a str,
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "csharp" {
        return Ok(());
    }
    let excluded = non_namespace_import_sites(target, file, cancelled)?;
    let namespace_start = index_scope_metadata(target, file, cancelled)?;
    let mut imports = HashSet::new();
    let mut scoped = false;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Import {
            continue;
        }
        scoped |= symbol.input.start_byte >= namespace_start;
        if excluded.contains(&(symbol.input.start_byte, symbol.input.end_byte)) || scoped {
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<String>() + symbol.name.len())),
        )?;
        imports.try_reserve(1).map_err(|_| StageItemFailure)?;
        imports.insert(try_clone_text(&symbol.name)?);
    }
    target
        .budget
        .charge(RESOLUTION_MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(
            size_of::<(FileId, NamespaceUsings)>() + file.file.file_id.as_str().len(),
        )))?;
    target
        .index
        .qualtype
        .csharp
        .by_file
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    target.index.qualtype.csharp.by_file.insert(
        file.file.file_id.clone(),
        NamespaceUsings {
            namespaces: imports,
            namespace_start,
            scoped,
        },
    );
    Ok(())
}

fn index_scope_metadata<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<u64, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut cutoff = u64::MAX;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind == SymbolKind::Namespace {
            cutoff = cutoff.min(symbol.input.start_byte);
        }
        if !nominal_candidate(symbol.kind) || symbol.input.qualified_name != symbol.name {
            continue;
        }
        let name = &symbol.name;
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<String>() + name.len())),
        )?;
        let names = &mut target.index.qualtype.csharp.root_types;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(try_clone_text(name)?);
    }
    Ok(cutoff)
}

fn non_namespace_import_sites<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<HashSet<(u64, u64)>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut excluded = HashSet::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        target
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(u64, u64)>()))?;
        excluded.try_reserve(1).map_err(|_| StageItemFailure)?;
        excluded.insert((binding.span.start_byte(), binding.span.end_byte()));
    }
    Ok(excluded)
}

pub(super) fn index_syntax<Cancel>(
    index: &mut ResolutionIndex,
    syntax: (&NativeFileFacts, &str),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, source) = syntax;
    if file.file.language != "csharp" {
        return Ok(());
    }
    let mut sites = HashSet::new();
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        let start = usize::try_from(reference.span.start_byte()).map_err(|_| StageItemFailure)?;
        let end = usize::try_from(reference.span.end_byte()).map_err(|_| StageItemFailure)?;
        if !source
            .get(start..end)
            .is_some_and(|text| text.starts_with("global::"))
        {
            continue;
        }
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<u64>()))?;
        sites.try_reserve(1).map_err(|_| StageItemFailure)?;
        sites.insert(reference.span.start_byte());
    }
    context
        .budget
        .charge(RESOLUTION_MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(
            size_of::<(FileId, HashSet<u64>)>() + file.file.file_id.as_str().len(),
        )))?;
    index
        .qualtype
        .csharp
        .global_sites
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    index
        .qualtype
        .csharp
        .global_sites
        .insert(file.file.file_id.clone(), sites);
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
    if !nominal(request.kind) {
        return Ok(None);
    }
    let Some(imports) = index
        .qualtype
        .csharp
        .by_file
        .get(request.file_id)
        .filter(|imports| !imports.scoped)
    else {
        return Ok(None);
    };
    let global = index
        .qualtype
        .csharp
        .global_sites
        .get(request.file_id)
        .is_some_and(|sites| sites.contains(&request.span.start_byte()));
    // Global root types have no qualified key distinct from the short-name bucket.
    if global && !request.name.contains('.') {
        return Ok(None);
    }
    // Relative qualifiers require namespace/type precedence beyond this subset.
    if !global
        && (request.name.contains('.')
            || super::qualtype_generics::blocked(index, request, cancelled)?)
    {
        return Ok(None);
    }
    let lookup = if global {
        Some(split_type_name(request.name))
    } else {
        type_name(request, imports.namespace_start, cancelled)?
    };
    let Some(lookup) = lookup else {
        return Ok(None);
    };
    let Some(enclosing) = enclosing_namespace(index, request, cancelled)? else {
        return Ok(None);
    };
    let selected = select_type(index, (request, imports, enclosing, lookup), cancelled)?;
    Ok(selected.resolution(PROVENANCE, 1.0))
}

fn select_type<'a, Cancel>(
    index: &'a ResolutionIndex,
    query: (&ResolutionRequest<'_>, &NamespaceUsings, &str, TypeName<'_>),
    cancelled: &mut Cancel,
) -> Result<Selection<'a>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, imports, enclosing, lookup) = query;
    let mut selected = Selection::default();
    let namespace = lookup.namespace.unwrap_or(enclosing);
    retain_qualified(
        &mut selected,
        (index, request, namespace, lookup.name),
        cancelled,
    )?;
    if lookup.namespace.is_some() || selected.candidate.is_some() {
        return Ok(selected);
    }
    // Enclosing namespaces and root types take precedence over root usings.
    // This path proves only a nearest match or imports in one namespace level.
    if enclosing.contains(['.', ':']) || index.qualtype.csharp.root_types.contains(lookup.name) {
        return Ok(selected);
    }
    for namespace in &imports.namespaces {
        retain_qualified(
            &mut selected,
            (index, request, namespace, lookup.name),
            cancelled,
        )?;
        if selected.ambiguous {
            break;
        }
    }
    Ok(selected)
}

fn retain_qualified<'a, Cancel>(
    selected: &mut Selection<'a>,
    lookup: (&'a ResolutionIndex, &ResolutionRequest<'_>, &str, &str),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (index, request, namespace, name) = lookup;
    if cancelled() {
        return Err(StageItemFailure);
    }
    // The empty namespace key is the global short-name bucket. Leave its
    // lookup to the existing base resolver rather than scanning it per site.
    if namespace.is_empty() {
        return Ok(());
    }
    let key = qualified_key(namespace, name)?;
    let Some(candidates) = index.candidates.get(&key) else {
        return Ok(());
    };
    for candidate in candidates.as_slice() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.qualified_name == key && eligible(index, request, candidate) {
            selected.retain(candidate);
            if selected.ambiguous {
                break;
            }
        }
    }
    Ok(())
}

fn qualified_key(namespace: &str, name: &str) -> Result<String, StageItemFailure> {
    let mut key = try_clone_text(namespace)?;
    key.try_reserve(name.len().saturating_add(2))
        .map_err(|_| StageItemFailure)?;
    if !namespace.is_empty() {
        key.push_str("::");
    }
    key.push_str(name);
    Ok(key)
}

fn type_name<'a, Cancel>(
    request: &ResolutionRequest<'a>,
    cutoff: u64,
    cancelled: &mut Cancel,
) -> Result<Option<TypeName<'a>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut alias = None;
    for binding in request.import_bindings.iter() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.span.start_byte() >= cutoff {
            continue;
        }
        if let Some(name) = binding_type_name(binding, request.name) {
            if alias.is_some() {
                return Ok(None);
            }
            alias = Some(name);
        }
    }
    Ok(Some(alias.unwrap_or_else(|| split_type_name(request.name))))
}

fn binding_type_name<'a>(
    binding: &'a ExtractedImportBinding,
    name: &'a str,
) -> Option<TypeName<'a>> {
    if binding.kind != ImportBindingKind::Namespace || binding.local_name != name {
        return None;
    }
    Some(split_type_name(&binding.module_specifier))
}

fn split_type_name(name: &str) -> TypeName<'_> {
    name.rsplit_once('.').map_or(
        TypeName {
            namespace: None,
            name,
        },
        |(namespace, name)| TypeName {
            namespace: Some(namespace),
            name,
        },
    )
}

fn eligible(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    candidate: &ResolutionCandidate,
) -> bool {
    reference_kind_candidate(request.kind, candidate)
        && nominal_candidate(candidate.kind)
        && index
            .modules
            .files
            .get(&candidate.file_id)
            .is_some_and(|file| file.language == "csharp")
        && (&candidate.file_id == request.file_id
            || candidate.visibility == Some(Visibility::Public))
        && candidate.parent_symbol_id.as_ref().is_none_or(|parent| {
            index
                .qualtype
                .owners
                .get(parent)
                .is_some_and(|parent| parent.kind == SymbolKind::Namespace)
        })
}

pub(super) fn enclosing_namespace<'a, Cancel>(
    index: &'a ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'a str>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut owner = request.owner;
    for _ in 0..=index.parents.len() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else { return Ok(Some("")) };
        if let Some(symbol) = index.qualtype.owners.get(id)
            && symbol.kind == SymbolKind::Namespace
        {
            return Ok(Some(&symbol.name));
        }
        owner = index.parents.get(id);
    }
    Ok(None)
}
