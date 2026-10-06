//! Parent/name method keys reuse the existing qualified candidate buckets.

use std::collections::HashMap;

use super::{
    NativeSymbolFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ResolveBudget, StageItemFailure, SymbolId,
    SymbolKind, try_clone_text, usize_to_u64,
};

type MethodNames = HashMap<String, Option<String>>;

#[derive(Default)]
pub(super) struct MethodIndex {
    by_parent: HashMap<SymbolId, MethodNames>,
}

impl MethodIndex {
    pub(super) fn key(&self, parent: &SymbolId, name: &str) -> Option<&str> {
        self.by_parent.get(parent)?.get(name)?.as_deref()
    }
}

#[derive(Clone, Copy)]
pub(super) struct MethodInput<'symbol> {
    pub(super) symbol: &'symbol NativeSymbolFacts,
    pub(super) parent: Option<&'symbol SymbolId>,
    pub(super) language: &'symbol str,
}

/// Called inside the symbol-index loop immediately after its cancellation poll.
pub(super) fn index_method(
    index: &mut MethodIndex,
    input: MethodInput<'_>,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    if !matches!(input.language, "apex" | "java" | "scala")
        || !matches!(input.symbol.kind, SymbolKind::Method | SymbolKind::Function)
    {
        return Ok(());
    }
    let Some(parent) = input.parent else {
        return Ok(());
    };
    if !index.by_parent.contains_key(parent) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(SymbolId, MethodNames)>() + parent.as_str().len()),
        )?;
        index.by_parent.insert(parent.clone(), HashMap::new());
    }
    let methods = index.by_parent.get_mut(parent).ok_or(StageItemFailure)?;
    let name = &input.symbol.name;
    let qualified = &input.symbol.input.qualified_name;
    if let Some(existing) = methods.get_mut(name) {
        if existing.as_deref().is_some_and(|known| known != qualified) {
            *existing = None;
        }
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(size_of::<(String, Option<String>)>() + name.len() + qualified.len()),
    )?;
    methods.insert(try_clone_text(name)?, Some(try_clone_text(qualified)?));
    Ok(())
}
