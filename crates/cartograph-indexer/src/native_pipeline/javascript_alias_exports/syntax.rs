//! Admit a plain named export clause only when its verified source proves a
//! runtime export. Unsupported syntax preserves the base resolver's result.

use super::super::{
    NativeFileFacts, NativeSymbolFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndex,
    ResolutionIndexContext, StageItemFailure, SymbolId, SymbolKind, javascript_family_name,
    qualtype_source, size_of, usize_to_u64,
};
use std::collections::HashMap;

pub(in super::super) fn index_file<Cancel>(
    index: &mut ResolutionIndex,
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_family_name(&file.file.language) {
        return Ok(());
    }
    let lines = alias_lines(file, context)?;
    if lines.is_empty() {
        return Ok(());
    }
    let Some(snapshot) = qualtype_source::read(file, context)? else {
        return Ok(());
    };
    for (position, line) in snapshot.source().lines().enumerate() {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        let position = u32::try_from(position + 1).map_err(|_| StageItemFailure)?;
        let Some(aliases) = lines.get(&position) else {
            continue;
        };
        let Some(kind) = line_kind(line) else {
            continue;
        };
        for alias in aliases {
            if (context.cancelled)() {
                return Err(StageItemFailure);
            }
            let Some(specifier) = alias_source(snapshot.source(), alias) else {
                continue;
            };
            record_kind(
                index,
                (&alias.input.symbol_id, specifier_kind(kind, specifier)),
                context,
            )?;
        }
    }
    Ok(())
}

fn record_kind<Cancel>(
    index: &mut ResolutionIndex,
    (owner, kind): (&SymbolId, ClauseKind),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let target = match kind {
        ClauseKind::Runtime => &mut index.javascript.aliases.runtime,
        ClauseKind::TypeOnly => &mut index.javascript.aliases.type_only,
    };
    context.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<SymbolId>() + owner.as_str().len()),
    )?;
    target.try_reserve(1).map_err(|_| StageItemFailure)?;
    target.insert(owner.clone());
    Ok(())
}

fn alias_lines<'file, Cancel>(
    file: &'file NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<u32, Vec<&'file NativeSymbolFacts>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let counts = alias_counts(file, context)?;
    let mut lines = reserve_lines(counts, context)?;
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Export {
            continue;
        }
        lines
            .get_mut(&symbol.input.start_line)
            .ok_or(StageItemFailure)?
            .push(symbol);
    }
    Ok(lines)
}

fn alias_counts<Cancel>(
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<u32, usize>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Export {
            continue;
        }
        let line = symbol.input.start_line;
        if let Some(count) = counts.get_mut(&line) {
            *count = count.checked_add(1).ok_or(StageItemFailure)?;
            continue;
        }
        context
            .budget
            .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<(u32, usize)>()))?;
        counts.try_reserve(1).map_err(|_| StageItemFailure)?;
        counts.insert(line, 1);
    }
    Ok(counts)
}

fn reserve_lines<'file, Cancel>(
    counts: HashMap<u32, usize>,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<u32, Vec<&'file NativeSymbolFacts>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let bytes = usize_to_u64(counts.len())
        .checked_mul(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(u32, Vec<&NativeSymbolFacts>)>()),
        )
        .ok_or(StageItemFailure)?;
    context.budget.charge(bytes)?;
    let mut lines = HashMap::new();
    lines
        .try_reserve(counts.len())
        .map_err(|_| StageItemFailure)?;
    for (line, count) in counts {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        let bytes = count
            .checked_mul(size_of::<&NativeSymbolFacts>())
            .ok_or(StageItemFailure)?;
        context.budget.charge(usize_to_u64(bytes))?;
        let mut group = Vec::new();
        group
            .try_reserve_exact(count)
            .map_err(|_| StageItemFailure)?;
        lines.insert(line, group);
    }
    Ok(lines)
}

#[derive(Clone, Copy)]
enum ClauseKind {
    Runtime,
    TypeOnly,
}

fn alias_source<'source>(source: &'source str, alias: &NativeSymbolFacts) -> Option<&'source str> {
    let start = usize::try_from(alias.input.start_byte).ok()?;
    let end = usize::try_from(alias.input.end_byte).ok()?;
    source.get(start..end).filter(|text| !text.contains('\n'))
}

fn line_kind(line: &str) -> Option<ClauseKind> {
    if unsupported_export_line(line) {
        return None;
    }
    let clause = line.trim_start().strip_prefix("export")?.trim_start();
    if clause
        .strip_prefix("type")
        .is_some_and(|tail| tail.trim_start().starts_with('{'))
    {
        return Some(ClauseKind::TypeOnly);
    }
    if !clause.starts_with('{') {
        return None;
    }
    Some(ClauseKind::Runtime)
}

fn unsupported_export_line(line: &str) -> bool {
    unsupported_export_text(line)
        || line.match_indices("export").nth(1).is_some()
        || !line.contains('}')
}

fn unsupported_export_text(line: &str) -> bool {
    !line.is_ascii() || line.contains(['\\', '\r']) || line.contains("/*") || line.contains("//")
}

fn specifier_kind(kind: ClauseKind, specifier: &str) -> ClauseKind {
    let mut words = specifier.split_whitespace();
    if matches!(kind, ClauseKind::TypeOnly)
        || (words.next() == Some("type") && words.next().is_some_and(|word| word != "as"))
    {
        ClauseKind::TypeOnly
    } else {
        ClauseKind::Runtime
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{ResolveBudget, SourceRoot};
    use super::*;

    #[test]
    fn a_large_group_reserves_its_full_capacity_and_charges_the_pointer_storage() {
        const ALIAS_COUNT: usize = 4_096;
        const LINE: u32 = 1;
        const BUDGET_BYTES: u64 = 1024 * 1024;
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("group root: {error}"));
        let source_root = SourceRoot::open(directory.path())
            .unwrap_or_else(|error| panic!("group root: {error}"));
        let mut budget =
            ResolveBudget::new(0, BUDGET_BYTES).unwrap_or_else(|_| panic!("group budget"));
        let lines = reserve_lines(
            HashMap::from([(LINE, ALIAS_COUNT)]),
            &mut ResolutionIndexContext {
                source_root: &source_root,
                budget: &mut budget,
                cancelled: &mut || false,
            },
        )
        .unwrap_or_else(|_| panic!("group reservation"));
        let group = lines.get(&LINE).unwrap_or_else(|| panic!("reserved group"));
        assert!(group.is_empty());
        assert_eq!(group.capacity(), ALIAS_COUNT);
        let expected = RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(u32, Vec<&NativeSymbolFacts>)>()
                    + ALIAS_COUNT * size_of::<&NativeSymbolFacts>(),
            );
        assert_eq!(budget.charged_bytes, expected);
        let mut tight =
            ResolveBudget::new(0, expected - 1).unwrap_or_else(|_| panic!("tight budget"));
        assert!(
            reserve_lines(
                HashMap::from([(LINE, ALIAS_COUNT)]),
                &mut ResolutionIndexContext {
                    source_root: &source_root,
                    budget: &mut tight,
                    cancelled: &mut || false,
                }
            )
            .is_err()
        );
    }
}
