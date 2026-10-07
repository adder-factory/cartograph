//! Retain inheritance declarations and their import scope under the shared budget.

use super::{
    ExtractedImportBinding, ExtractedReference, NativeFileFacts, ParentDeclaration, ParentFile,
    ResolutionIndexTarget, StageItemFailure, SymbolId, size_of, try_clone_text, usize_to_u64,
};

pub(super) fn declaration(
    target: &mut ResolutionIndexTarget<'_>,
    (reference, owner, name): (&ExtractedReference, &SymbolId, &str),
    declarations: &mut Vec<ParentDeclaration>,
) -> Result<(), StageItemFailure> {
    target.budget.charge(
        usize_to_u64(size_of::<ParentDeclaration>())
            .saturating_add(usize_to_u64(name.len()))
            .saturating_add(usize_to_u64(owner.as_str().len())),
    )?;
    declarations
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    declarations.push(ParentDeclaration {
        owner: owner.clone(),
        name: try_clone_text(name)?,
        span: reference.span,
        blocked: reference.resolution_name.as_deref().is_some_and(|name| {
            name.starts_with(super::super::PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX)
        }),
        kind: reference.kind,
    });
    Ok(())
}

pub(super) fn retain<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    (file, declarations): (&NativeFileFacts, Vec<ParentDeclaration>),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if declarations.is_empty() {
        return Ok(());
    }
    let mut imports = Vec::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        target.budget.charge(
            usize_to_u64(size_of::<ExtractedImportBinding>())
                .saturating_add(usize_to_u64(binding.module_specifier.len()))
                .saturating_add(usize_to_u64(binding.imported_name.len()))
                .saturating_add(usize_to_u64(binding.local_name.len())),
        )?;
        imports.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
        imports.push(ExtractedImportBinding {
            kind: binding.kind,
            module_specifier: try_clone_text(&binding.module_specifier)?,
            imported_name: try_clone_text(&binding.imported_name)?,
            local_name: try_clone_text(&binding.local_name)?,
            span: binding.span,
        });
    }
    target.budget.charge(
        usize_to_u64(size_of::<ParentFile>())
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
    )?;
    target
        .index
        .receivers
        .pending
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    target.index.receivers.pending.push(ParentFile {
        file_id: file.file.file_id.clone(),
        imports,
        declarations,
    });
    Ok(())
}
