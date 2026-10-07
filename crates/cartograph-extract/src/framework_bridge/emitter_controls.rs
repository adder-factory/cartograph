//! Control-method exclusions need original, typed native-emitter inheritance evidence.

use super::{BTreeMap, BTreeSet, ExtractError, FrameworkBuilder, ReferenceKind, SymbolKind};

const ENTRY_BYTES: u64 = 256;
const EMITTER_BASE: &str = "RCTEventEmitter";

pub(super) fn objc_classes(
    builder: &mut FrameworkBuilder<'_, '_>,
) -> Result<BTreeSet<String>, ExtractError> {
    let mut classes = BTreeMap::new();
    for index in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        if builder
            .original_symbol(index)
            .is_none_or(|symbol| symbol.kind != SymbolKind::Class)
        {
            continue;
        }
        builder.bridge.reserve_working_bytes(ENTRY_BYTES)?;
        if let Some(symbol) = builder.original_symbol(index) {
            classes.insert(symbol.id.clone(), index);
        }
    }
    let mut emitters = BTreeSet::new();
    for index in 0..builder.references().len() {
        builder.bridge.charge_work(1)?;
        let reference = &builder.references()[index];
        if reference.kind != ReferenceKind::Extends || reference.name != EMITTER_BASE {
            continue;
        }
        let Some(class) = reference
            .owner
            .as_ref()
            .and_then(|owner| classes.get(owner))
            .copied()
        else {
            continue;
        };
        let Some(symbol) = builder.original_symbol(class) else {
            continue;
        };
        let bytes = u64::try_from(symbol.name.len()).map_err(|_| ExtractError::OutputLimit)?;
        builder.bridge.reserve_working_bytes(ENTRY_BYTES + bytes)?;
        if let Some(symbol) = builder.original_symbol(class) {
            emitters.insert(symbol.name.clone());
        }
    }
    Ok(emitters)
}
