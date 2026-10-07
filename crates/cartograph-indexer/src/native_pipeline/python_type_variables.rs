//! A variable is a type only when its declaration calls the unshadowed, named
//! `typing.TypeVar` import. This is not general value or annotation inference.

use std::collections::HashSet;

use super::{
    ExtractedReference, HashMap, ImportBindingKind, ImportReferenceSite, ImportResolution,
    ImportResolutionRequest, LexicalScopeQuery, NativeFileFacts, NativeSymbolFacts,
    PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind,
    ReferenceResolution, ResolutionIndex, ResolutionIndexContext, ResolutionIndexTarget,
    ResolutionRequest, StageItemFailure, SymbolId, SymbolKind, resolution_candidates_for_file,
    resolve_import, resolve_lexical_scope, size_of, usize_to_u64,
};

pub(super) const PROVENANCE: &str = "native-python-typevar";
const FACTORY_SYNTAX_BYTES: usize = 256;

#[derive(Default)]
pub(super) struct TypeVariables {
    parameters: HashSet<SymbolId>,
    project_typing: bool,
}

pub(super) fn index_module(target: &mut ResolutionIndexTarget<'_>, file: &NativeFileFacts) {
    let path = file.file.normalized_path.as_str();
    // Without Python path/provider evidence, a project namesake prevents us
    // from treating this factory as the standard-library typing.TypeVar.
    target.index.qualtype.python.project_typing |= file.file.language == "python"
        && (path == "typing.py"
            || path.ends_with("/typing.py")
            || path == "typing/__init__.py"
            || path.ends_with("/typing/__init__.py"));
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
    let mut imports = typevar_imports(
        &mut ResolutionIndexTarget {
            index,
            budget: context.budget,
        },
        file,
        context.cancelled,
    )?;
    if !imports.values().any(|valid| *valid) {
        return Ok(());
    }
    let variables = variable_declarations(file, &mut imports, context)?;
    let Some(snapshot) = super::qualtype_source::read(file, context)? else {
        return Ok(());
    };
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if reference.kind != ReferenceKind::Calls
            || imports.get(reference.name.as_str()) != Some(&true)
            || reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.starts_with(PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX))
        {
            continue;
        }
        let Some(variable) = reference.owner.as_ref().and_then(|id| variables.get(id)) else {
            continue;
        };
        if !direct_factory_assignment(variable, reference, snapshot.source()) {
            continue;
        }
        let id = &variable.input.symbol_id;
        context.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<SymbolId>()))
                .saturating_add(usize_to_u64(id.as_str().len())),
        )?;
        index
            .qualtype
            .python
            .parameters
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        index.qualtype.python.parameters.insert(id.clone());
    }
    Ok(())
}

fn variable_declarations<'file, Cancel>(
    file: &'file NativeFileFacts,
    imports: &mut HashMap<&str, bool>,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<&'file SymbolId, &'file NativeSymbolFacts>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut variables = HashMap::new();
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Import
            && let Some(valid) = imports.get_mut(symbol.name.as_str())
        {
            *valid = false;
        }
        if symbol.kind != SymbolKind::Variable {
            continue;
        }
        context.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(&SymbolId, &NativeSymbolFacts)>())),
        )?;
        variables.try_reserve(1).map_err(|_| StageItemFailure)?;
        variables.insert(&symbol.input.symbol_id, symbol);
    }
    Ok(variables)
}

// This exact subset is an unannotated direct assignment of TypeVar("T") to T.
// Constraints, namespace factories and computed/container values need richer
// expression facts; merely owning a TypeVar call does not make a value a type.
fn direct_factory_assignment(
    variable: &NativeSymbolFacts,
    reference: &ExtractedReference,
    source: &str,
) -> bool {
    let Some(prefix) = source_range(
        source,
        variable.input.start_byte,
        reference.span.start_byte(),
    ) else {
        return false;
    };
    let Some(suffix) = source_range(source, reference.span.end_byte(), variable.input.end_byte)
    else {
        return false;
    };
    if prefix.len() > FACTORY_SYNTAX_BYTES
        || suffix.len() > FACTORY_SYNTAX_BYTES
        || source_range(
            source,
            reference.span.start_byte(),
            reference.span.end_byte(),
        ) != Some(reference.name.as_str())
        || prefix
            .strip_prefix(&variable.name)
            .is_none_or(|tail| tail.trim() != "=")
    {
        return false;
    }
    let literal = suffix
        .trim()
        .strip_prefix('(')
        .and_then(|tail| tail.strip_suffix(')'));
    literal.and_then(|literal| quoted_name(literal.trim())) == Some(variable.name.as_str())
}

fn source_range(source: &str, start: u64, end: u64) -> Option<&str> {
    source.get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
}

fn quoted_name(value: &str) -> Option<&str> {
    let quote = char::from(*value.as_bytes().first()?);
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    value.strip_prefix(quote)?.strip_suffix(quote)
}

fn typevar_imports<'a, Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &'a NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<HashMap<&'a str, bool>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut imports = HashMap::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(size_of::<(&str, bool)>())),
        )?;
        imports.try_reserve(1).map_err(|_| StageItemFailure)?;
        let name = binding.local_name.as_str();
        let desired = binding.kind == ImportBindingKind::Named
            && binding.module_specifier == "typing"
            && binding.imported_name == "TypeVar";
        if let Some(previous) = imports.get_mut(name) {
            *previous = false;
        } else {
            imports.insert(name, desired);
        }
    }
    Ok(imports)
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(request.kind, ReferenceKind::TypeOf | ReferenceKind::Returns)
        || request.import_bindings.fallback_blocked
        || index.qualtype.python.project_typing
    {
        return Ok(None);
    }
    let query = ResolutionRequest {
        kind: ReferenceKind::References,
        ..*request
    };
    let local = resolve_lexical_scope(
        index,
        LexicalScopeQuery {
            request: &query,
            candidates: resolution_candidates_for_file(index, query.name, query.file_id),
        },
        cancelled,
    )?;
    let target = if local.is_some() {
        local
    } else {
        match resolve_import(
            index,
            ImportResolutionRequest {
                reference: &query,
                site: ImportReferenceSite::Usage,
            },
            cancelled,
        )? {
            ImportResolution::Resolved(target) => Some(target),
            ImportResolution::NotBound | ImportResolution::Unresolved => None,
        }
    };
    Ok(target
        .filter(|target| index.qualtype.python.parameters.contains(&target.symbol_id))
        .map(|mut target| {
            target.provenance = PROVENANCE;
            ReferenceResolution::resolved(target)
        }))
}
