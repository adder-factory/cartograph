//! Alias and bare resources consume the native Python import fence. Its
//! conservative file-wide boundary covers parameters, assignments, patterns,
//! attribute writes and nested imports without adding another lexical resolver.
use super::{Imports, reserve_entry};
use crate::{ExtractError, PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX, framework::FrameworkBuilder};

pub(super) fn block_shadowed_names(
    builder: &mut FrameworkBuilder<'_, '_>,
    imports: &mut Imports<'_>,
) -> Result<(), ExtractError> {
    for ordinal in 0..builder.reference_count() {
        builder.bridge.charge_work(1)?;
        let Some(reference) = builder.reference(ordinal) else {
            continue;
        };
        if reference
            .resolution_name
            .as_deref()
            .is_none_or(|lookup| !lookup.starts_with(PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX))
        {
            continue;
        }
        let name = reference
            .name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_owned();
        block_name(builder, imports, name)?;
    }
    for ordinal in 0..builder.bridge.file.import_bindings.len() {
        builder.bridge.charge_work(1)?;
        let binding = &builder.bridge.file.import_bindings[ordinal];
        if binding.module_specifier == "neug" {
            continue;
        }
        let name = binding.local_name.clone();
        block_name(builder, imports, name)?;
    }
    Ok(())
}

fn block_name(
    builder: &mut FrameworkBuilder<'_, '_>,
    imports: &mut Imports<'_>,
    name: String,
) -> Result<(), ExtractError> {
    if (!imports.modules.contains(name.as_str()) && !imports.constructors.contains(name.as_str()))
        || imports.blocked.contains(&name)
    {
        return Ok(());
    }
    reserve_entry(builder, &name)?;
    imports.blocked.insert(name);
    Ok(())
}
