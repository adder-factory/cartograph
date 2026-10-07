//! Imported class calls follow only directly declared, standard decorated methods.

use std::collections::{HashMap, HashSet};

use super::python_resolution::ImportQuery;
use super::{
    NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceDispatch, ReferenceKind,
    ResolutionCandidate, ResolutionIndex, ResolutionIndexContext, ResolvedTarget, StageItemFailure,
    SymbolId, SymbolKind, qualtype_source, resolution_candidates_for_file, select_candidate,
    size_of, usize_to_u64,
};

mod classes;
mod syntax;

const PROVENANCE: &str = "native-python-imported-class-member";
const CONFIDENCE: f32 = 0.9;
const DECORATORS: [&str; 2] = ["classmethod", "staticmethod"];

#[derive(Default)]
pub(super) struct Members {
    callable: HashSet<SymbolId>,
    classes: classes::Classes,
}

struct MethodPolicy<'file> {
    decorators: HashMap<&'file SymbolId, bool>,
    attributes: HashSet<&'file str>,
    occurrences: HashMap<&'file str, usize>,
}

pub(super) fn index_file<Cancel>(
    index: &mut ResolutionIndex,
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "python" {
        return Ok(());
    }
    let Some(snapshot) = qualtype_source::read(file, context)? else {
        return Ok(());
    };
    // Keep the source subset conservative: an asterisk can introduce an
    // unmodeled wildcard import binding for either builtin decorator.
    if !snapshot.source().is_ascii() || snapshot.source().contains('*') {
        return Ok(());
    }
    if decorator_binding(file, context.cancelled)? {
        return Ok(());
    }
    let sites = decorator_sites((file, snapshot.source()), context)?;
    let attributes = attributes(file, context)?;
    if !clean_decorator_names(snapshot.source(), &sites, context.cancelled)? {
        return Ok(());
    }
    let decorators = syntax::decorated_owners(
        file,
        |reference| {
            Ok(sites.contains(
                &usize::try_from(reference.span.start_byte()).map_err(|_| StageItemFailure)?,
            ))
        },
        context,
    )?;
    let occurrences = syntax::method_occurrences((file, snapshot.source()), context)?;
    classes::index_file(
        &mut index.qualtype.python_class_members.classes,
        (file, snapshot.source()),
        context,
    )?;
    publish_methods(
        index,
        (
            file,
            MethodPolicy {
                decorators,
                attributes,
                occurrences,
            },
        ),
        context,
    )
}

fn decorator_binding<Cancel>(
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if DECORATORS.contains(&binding.local_name.as_str()) {
            return Ok(true);
        }
    }
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if DECORATORS.contains(&symbol.name.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn publish_methods<Cancel>(
    index: &mut ResolutionIndex,
    (file, policy): (&NativeFileFacts, MethodPolicy<'_>),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        let owner = &symbol.input.symbol_id;
        if symbol.kind != SymbolKind::Method
            || policy.decorators.get(owner) != Some(&true)
            || policy.attributes.contains(symbol.name.as_str())
            || policy.occurrences.get(symbol.name.as_str()) != Some(&syntax::UNIQUE_OCCURRENCE)
        {
            continue;
        }
        context.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<SymbolId>() + owner.as_str().len()),
        )?;
        index
            .qualtype
            .python_class_members
            .callable
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        index
            .qualtype
            .python_class_members
            .callable
            .insert(owner.clone());
    }
    Ok(())
}

fn attributes<'file, Cancel>(
    file: &'file NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashSet<&'file str>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut names = HashSet::new();
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if reference.kind != ReferenceKind::FieldAccess {
            continue;
        }
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<&str>()))?;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(reference.name.as_str());
    }
    Ok(names)
}

fn decorator_sites<Cancel>(
    (file, source): (&NativeFileFacts, &str),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashSet<usize>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut sites = HashSet::new();
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if reference.kind != ReferenceKind::Decorates
            || !DECORATORS.contains(&reference.name.as_str())
            || !syntax::bare_decorator(source, reference)
        {
            continue;
        }
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<usize>()))?;
        sites.try_reserve(1).map_err(|_| StageItemFailure)?;
        sites.insert(usize::try_from(reference.span.start_byte()).map_err(|_| StageItemFailure)?);
    }
    Ok(sites)
}

fn clean_decorator_names<Cancel>(
    source: &str,
    sites: &HashSet<usize>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for decorator in DECORATORS {
        for (start, _) in source.match_indices(decorator) {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if !sites.contains(&start)
                || source
                    .as_bytes()
                    .get(start.checked_sub(1).unwrap_or(source.len()))
                    != Some(&b'@')
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub(super) fn resolve<Cancel>(
    query: ImportQuery<'_, '_>,
    class: &ResolutionCandidate,
    cancelled: &mut Cancel,
) -> Result<super::ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let request = query.input.reference;
    if class.kind != SymbolKind::Class
        || request.kind != ReferenceKind::Calls
        || request.dispatch != ReferenceDispatch::Static
        || !classes::allowed(query, class)
    {
        return Ok(super::ImportResolution::Unresolved);
    }
    let member = request
        .name
        .strip_prefix(&query.binding.local_name)
        .and_then(|suffix| suffix.strip_prefix('.'));
    let Some(member) = member.filter(|member| !member.is_empty() && !member.contains(['.', ':']))
    else {
        return Ok(super::ImportResolution::Unresolved);
    };
    let candidate = select_candidate(
        resolution_candidates_for_file(query.index, member, &class.file_id),
        |candidate| {
            candidate.kind == SymbolKind::Method
                && candidate.parent_symbol_id.as_ref() == Some(&class.symbol_id)
                && query
                    .index
                    .qualtype
                    .python_class_members
                    .callable
                    .contains(&candidate.symbol_id)
        },
        cancelled,
    )?;
    Ok(
        candidate.map_or(super::ImportResolution::Unresolved, |candidate| {
            super::ImportResolution::Resolved(ResolvedTarget {
                symbol_id: candidate.symbol_id.clone(),
                kind: candidate.kind,
                confidence: CONFIDENCE,
                provenance: PROVENANCE,
            })
        }),
    )
}
