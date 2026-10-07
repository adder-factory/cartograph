//! Keep typed framework intent at an occurrence without changing its reference.

use super::{FrameworkBuilder, safe_signal};
use crate::{
    ExtractError, ExtractedImportBinding, ImportBindingKind, budget::import_binding_budget_bytes,
    source_lines::SourceByteRange,
};

pub(crate) fn append(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, &str, usize, usize),
) -> Result<(), ExtractError> {
    let (module, name, start, end) = input;
    let Some(name) = safe_signal(name) else {
        return Ok(());
    };
    let binding = ExtractedImportBinding {
        kind: ImportBindingKind::Named,
        module_specifier: module.to_owned(),
        imported_name: name.clone(),
        local_name: name,
        span: builder
            .lines
            .span(SourceByteRange::new(start, end, builder.source().len()))?,
    };
    builder.bridge.budget.reserve_fact(
        import_binding_budget_bytes(&binding),
        [
            binding.module_specifier.as_str(),
            binding.imported_name.as_str(),
        ],
    )?;
    builder
        .bridge
        .file
        .import_bindings
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.bridge.file.import_bindings.push(binding);
    Ok(())
}
