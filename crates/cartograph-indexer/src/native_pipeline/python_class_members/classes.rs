//! Admit plain class headers and one proven standard bare dataclass decorator.

use std::collections::{HashMap, HashSet};

use super::super::{
    ImportBindingKind, ModuleResolutionRequest, NativeFileFacts, NativeSymbolFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind, ResolutionCandidate, ResolutionIndexContext,
    StageItemFailure, SymbolId, SymbolKind, python_resolution, size_of, usize_to_u64,
};
use super::syntax;
use python_resolution::ImportQuery;

const DATACLASS: &str = "dataclass";
const MODULE: &str = "dataclasses";
const MAX_CLASS_HEADER_BYTES: usize = 1_024;
const MAX_PREFIX_LINES: usize = 2;
const CUSTOM_CLASS_BEHAVIOR: [&str; 3] = ["metaclass", "__init_subclass__", "__getattribute__"];
const DECORATOR_CLOSERS: [char; 4] = [')', ']', '}', '\\'];

#[derive(Default)]
pub(super) struct Classes {
    admitted: HashMap<SymbolId, bool>,
}

pub(super) fn index_file<Cancel>(
    classes: &mut Classes,
    (file, source): (&NativeFileFacts, &str),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if custom_class_behavior(source, context)? {
        return Ok(());
    }
    let standard = standard_dataclass((file, source), context)?;
    let decorations =
        syntax::decorated_owners(
            file,
            |reference| {
                Ok(standard
                    && reference.name == DATACLASS
                    && syntax::bare_decorator(source, reference))
            },
            context,
        )?;
    let roots = plain_roots((file, source), &decorations, context)?;
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Class {
            continue;
        }
        let owner = &symbol.input.symbol_id;
        let decorator = decorations.get(owner).copied();
        if admitted_class((source, symbol), decorator, &roots) {
            publish_class(classes, (owner, decorator == Some(true)), context)?;
        }
    }
    Ok(())
}

/// Metaclasses and attribute hooks can rewrite member lookup for every class.
fn custom_class_behavior<Cancel>(
    source: &str,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for marker in CUSTOM_CLASS_BEHAVIOR {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if source.contains(marker) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A class is admitted when its decorators are proven and every base is a
/// plain admitted root.
fn admitted_class(
    (source, symbol): (&str, &NativeSymbolFacts),
    decorator: Option<bool>,
    roots: &HashMap<&str, bool>,
) -> bool {
    if decorator == Some(false) || !decorator_prefix((source, symbol), decorator) {
        return false;
    }
    let Some(bases) = header_bases(source, symbol) else {
        return false;
    };
    bases.is_empty()
        || bases
            .split(',')
            .all(|base| roots.get(base.trim()) == Some(&true))
}

fn publish_class<Cancel>(
    classes: &mut Classes,
    (owner, standard): (&SymbolId, bool),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    context.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(size_of::<(SymbolId, bool)>() + owner.as_str().len()),
    )?;
    classes
        .admitted
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    classes.admitted.insert(owner.clone(), standard);
    Ok(())
}

fn plain_roots<'file, Cancel>(
    (file, source): (&'file NativeFileFacts, &str),
    decorations: &HashMap<&SymbolId, bool>,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<&'file str, bool>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut roots = HashMap::new();
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.input.qualified_name != symbol.name {
            continue;
        }
        let valid = symbol.kind == SymbolKind::Class
            && !decorations.contains_key(&symbol.input.symbol_id)
            && header_bases(source, symbol) == Some("")
            && decorator_prefix((source, symbol), None);
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(&str, bool)>()))?;
        roots.try_reserve(1).map_err(|_| StageItemFailure)?;
        roots
            .entry(symbol.name.as_str())
            .and_modify(|valid: &mut bool| *valid = false)
            .or_insert(valid);
    }
    for binding in &file.import_bindings {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if let Some(valid) = roots.get_mut(binding.local_name.as_str()) {
            *valid = false;
        }
    }
    Ok(roots)
}

fn decorator_prefix((source, symbol): (&str, &NativeSymbolFacts), decorator: Option<bool>) -> bool {
    let Ok(start) = usize::try_from(symbol.input.start_byte) else {
        return false;
    };
    let lower = start.saturating_sub(MAX_CLASS_HEADER_BYTES);
    let Some(prefix) = source.get(lower..start) else {
        return false;
    };
    let (history, indentation) = prefix.rsplit_once('\n').unwrap_or(("", prefix));
    if !indentation.trim().is_empty()
        || (lower > 0 && history.match_indices('\n').nth(MAX_PREFIX_LINES).is_none())
    {
        return false;
    }
    let mut lines = history
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    if decorator == Some(true) && lines.next() != Some("@dataclass") {
        return false;
    }
    lines
        .take(MAX_PREFIX_LINES)
        .all(|line| !line.starts_with('@') && !line.ends_with(DECORATOR_CLOSERS))
}

fn standard_dataclass<Cancel>(
    (file, source): (&NativeFileFacts, &str),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(import) = dataclass_import(file, context.cancelled)? else {
        return Ok(false);
    };
    let mut sites = HashSet::new();
    insert_site(&mut sites, import, context)?;
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if reference.kind == ReferenceKind::Decorates
            && reference.name == DATACLASS
            && syntax::bare_decorator(source, reference)
        {
            insert_site(
                &mut sites,
                usize::try_from(reference.span.start_byte()).map_err(|_| StageItemFailure)?,
                context,
            )?;
        }
    }
    let mut offset = 0;
    for chunk in source.split_inclusive(|character| !syntax::identifier(character)) {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        let word = chunk.trim_end_matches(|character| !syntax::identifier(character));
        if word == DATACLASS && !sites.contains(&offset) {
            return Ok(false);
        }
        offset += chunk.len();
    }
    Ok(true)
}

fn dataclass_import<Cancel>(
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<Option<usize>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut site = None;
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.local_name != DATACLASS {
            continue;
        }
        if site.is_some()
            || binding.kind != ImportBindingKind::Named
            || binding.module_specifier != MODULE
            || binding.imported_name != DATACLASS
        {
            return Ok(None);
        }
        site = Some(usize::try_from(binding.span.start_byte()).map_err(|_| StageItemFailure)?);
    }
    Ok(site)
}

fn insert_site<Cancel>(
    sites: &mut HashSet<usize>,
    site: usize,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    context
        .budget
        .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<usize>()))?;
    sites.try_reserve(1).map_err(|_| StageItemFailure)?;
    sites.insert(site);
    Ok(())
}

fn header_bases<'source>(source: &'source str, symbol: &NativeSymbolFacts) -> Option<&'source str> {
    let suffix = class_header(source, symbol)?.trim();
    if suffix.is_empty() {
        return Some("");
    }
    suffix.strip_circumfix('(', ')').filter(|bases| {
        bases.is_empty() || bases.split(',').all(|base| simple_identifier(base.trim()))
    })
}

fn class_header<'source>(source: &'source str, symbol: &NativeSymbolFacts) -> Option<&'source str> {
    let start = usize::try_from(symbol.input.start_byte).ok()?;
    let end = usize::try_from(symbol.input.end_byte).ok()?;
    source
        .get(start..end.min(start.saturating_add(MAX_CLASS_HEADER_BYTES)))
        .and_then(|source| source.lines().next())
        .and_then(|header| header.trim_end().strip_suffix(':'))
        .and_then(|header| header.strip_prefix("class "))
        .and_then(|header| header.strip_prefix(&symbol.name))
}

fn simple_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(super) fn allowed(query: ImportQuery<'_, '_>, class: &ResolutionCandidate) -> bool {
    let Some(standard) = query
        .index
        .qualtype
        .python_class_members
        .classes
        .admitted
        .get(&class.symbol_id)
    else {
        return false;
    };
    !standard
        || !python_resolution::project_module_may_exist(
            &query.index.modules,
            ModuleResolutionRequest {
                importing_path: query.input.reference.file_path,
                importing_language: "python",
                specifier: MODULE,
            },
        )
}
