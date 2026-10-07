//! Hook implementations reference a shared contract resource in their file.

use std::collections::BTreeMap;

use super::{
    ExtractError, FrameworkBuilder, FrameworkReferenceInput, LandmarkInput, MAX_SIGNAL_BYTES,
    ReferenceKind, SymbolId, SymbolKind, documented_hook,
};

const HOOK_INDEX_ENTRY_BYTES: u64 = 256;

pub(super) fn scan(builder: &mut FrameworkBuilder<'_, '_>) -> Result<(), ExtractError> {
    let Some(module) = module_name(builder.path()) else {
        return Ok(());
    };
    let mut contracts = BTreeMap::new();
    for index in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        let Some(implementation) = implementation(builder, (index, &module)) else {
            continue;
        };
        add_contract(builder, &mut contracts, implementation)?;
    }
    Ok(())
}

fn module_name(path: &str) -> Option<String> {
    let (stem, extension) = path.rsplit('/').next()?.rsplit_once('.')?;
    ["module", "install", "theme", "inc"]
        .contains(&extension.to_ascii_lowercase().as_str())
        .then(|| stem.split('.').next().unwrap_or_default().replace('-', "_"))
}

struct Implementation {
    owner: SymbolId,
    contract: String,
    start: usize,
    end: usize,
}

fn implementation(
    builder: &FrameworkBuilder<'_, '_>,
    query: (usize, &str),
) -> Option<Implementation> {
    let (index, module) = query;
    let symbol = builder.original_symbol(index)?;
    if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Module) {
        return None;
    }
    let start = usize::try_from(symbol.span.start_byte()).ok()?;
    let end = usize::try_from(symbol.span.end_byte()).ok()?;
    let source = builder.source();
    let mut prefix_start = start.saturating_sub(MAX_SIGNAL_BYTES);
    while !source.is_char_boundary(prefix_start) {
        prefix_start += 1;
    }
    let contract = documented_hook(&source[prefix_start..start]).or_else(|| {
        symbol
            .name
            .strip_prefix(module)?
            .strip_prefix('_')
            .filter(|suffix| !suffix.is_empty())
            .map(|suffix| format!("hook_{suffix}"))
    })?;
    let name_start = start + source[start..end].find(&symbol.name)?;
    Some(Implementation {
        owner: symbol.id.clone(),
        contract,
        start: name_start,
        end: name_start + symbol.name.len(),
    })
}

fn add_contract(
    builder: &mut FrameworkBuilder<'_, '_>,
    contracts: &mut BTreeMap<String, String>,
    implementation: Implementation,
) -> Result<(), ExtractError> {
    let contract = &implementation.contract;
    if !contracts.contains_key(contract) {
        builder
            .bridge
            .reserve_working_bytes(HOOK_INDEX_ENTRY_BYTES)?;
        let identity = format!("drupal-hook:{contract}");
        let Some(_) = builder.add_landmark_with_id(LandmarkInput {
            kind: SymbolKind::Resource,
            name: contract.clone(),
            identity: identity.clone(),
            start: 0,
            end: builder.source().chars().next().map_or(0, char::len_utf8),
            body_search_text: format!("drupal hook contract {contract}"),
            target: None,
        })?
        else {
            return Ok(());
        };
        contracts.insert(contract.clone(), format!("{}::{identity}", builder.path()));
    }
    builder.add_reference(FrameworkReferenceInput {
        owner: Some(implementation.owner),
        name: contract,
        resolution_name: contracts.get(contract).map(String::as_str),
        kind: ReferenceKind::References,
        start: implementation.start,
        end: implementation.end,
    })
}
