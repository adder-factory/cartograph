//! Build package membership once; reference lookup never scans wildcard imports.

use super::{
    ImportHints, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceResolution,
    ResolutionIndexTarget, StageItemFailure, TypeChoice, TypeQuery, UNRESOLVED_IMPORT_PROVENANCE,
    UNRESOLVED_PROVENANCE, UniqueName, WILDCARD_PROVENANCE, joined_name, language, nominal_type,
    record_unique, try_clone_text, type_choice, usize_to_u64,
};
use cartograph_domain::SymbolKind;

pub(super) fn index_package<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    input: (&NativeFileFacts, &ImportHints),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, hints) = input;
    if !language(&file.file.language) {
        return Ok(());
    }
    let package = hints.package.as_deref().unwrap_or_default();
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !(nominal_type(symbol.kind)
            || matches!(
                symbol.kind,
                SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Variable
                    | SymbolKind::Constant
                    | SymbolKind::Property
                    | SymbolKind::Field
            ))
            || target.index.parents.get(&symbol.input.symbol_id) != hints.package_owner.as_ref()
        {
            continue;
        }
        if !target.index.jvm.packages.contains_key(package) {
            target.budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<(
                        String,
                        std::collections::HashMap<String, Option<String>>,
                    )>()))
                    .saturating_add(usize_to_u64(package.len())),
            )?;
            target
                .index
                .jvm
                .packages
                .try_reserve(1)
                .map_err(|_| StageItemFailure)?;
            target
                .index
                .jvm
                .packages
                .insert(try_clone_text(package)?, std::collections::HashMap::new());
        }
        let names = target
            .index
            .jvm
            .packages
            .get_mut(package)
            .ok_or(StageItemFailure)?;
        record_unique(
            names,
            UniqueName {
                key: try_clone_text(&symbol.name)?,
                key_bytes: usize_to_u64(symbol.name.len()),
                value: joined_name(package, &symbol.name),
            },
            target.budget,
        )?;
    }
    Ok(())
}

pub(in crate::native_pipeline) fn prepare<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let jvm = &mut target.index.jvm;
    for hints in jvm.files.values_mut() {
        for package in &hints.wildcards {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let Some(names) = jvm.packages.get(package) else {
                continue;
            };
            for (name, key) in names {
                if cancelled() {
                    return Err(StageItemFailure);
                }
                record_unique(
                    &mut hints.wildcard_types,
                    UniqueName {
                        key: try_clone_text(name)?,
                        key_bytes: usize_to_u64(name.len()),
                        value: key.as_deref().map(try_clone_text).transpose()?,
                    },
                    target.budget,
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn resolve<Cancel>(
    query: TypeQuery<'_, '_, '_>,
    hints: &ImportHints,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (head, suffix) = query
        .request
        .name
        .split_once('.')
        .map_or((query.request.name, None), |(head, suffix)| {
            (head, Some(suffix))
        });
    let Some(key) = hints.wildcard_types.get(head) else {
        return Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE));
    };
    let Some(key) = key.as_deref() else {
        return Ok(ReferenceResolution::unresolved(
            UNRESOLVED_IMPORT_PROVENANCE,
        ));
    };
    let joined = suffix.and_then(|suffix| joined_name(key, suffix));
    if suffix.is_some() && joined.is_none() {
        return Ok(ReferenceResolution::unresolved(
            UNRESOLVED_IMPORT_PROVENANCE,
        ));
    }
    let query = TypeQuery {
        key: joined.as_deref().unwrap_or(key),
        ..query
    };
    match type_choice(query, cancelled)? {
        TypeChoice::Absent => Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)),
        choice => choice.resolution((query, WILDCARD_PROVENANCE), cancelled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_pipeline::{
        ExtractedReferenceQuery, FileDocumentIdentity, FileImportBindingIndex,
        FileResolutionContext, ImportBindingScratch, ReferenceLookup, ResolutionIndex,
        ResolveBudget, jvm_resolution::select_import_bindings, native_file_symbol_id,
        resolve_extracted_reference,
    };
    use cartograph_domain::{FileId, ReferenceKind, SourcePosition, SourceSpan};
    use cartograph_extract::{ExtractedImportBinding, ExtractedReference, ImportBindingKind};

    #[test]
    fn ten_thousand_absent_packages_and_names_have_a_linear_work_bound() {
        const COUNT: usize = 10_000;
        let file = FileId::from_uuid_v8([5; 16]);
        let span = SourceSpan::synthetic(
            SourcePosition::new(0, 1, 0).unwrap_or_else(|error| panic!("position: {error}")),
        );
        let bindings = wildcard_bindings(COUNT, span);
        let hints = ImportHints {
            wildcards: bindings
                .iter()
                .map(|binding| binding.module_specifier.clone())
                .collect(),
            ..ImportHints::default()
        };
        let mut index = ResolutionIndex::default();
        index.jvm.files.insert(file.clone(), hints);
        let mut budget =
            ResolveBudget::new(0, 8 * 1024 * 1024).unwrap_or_else(|_| panic!("budget"));
        let imports = FileImportBindingIndex::new(&bindings, &mut budget, "java")
            .unwrap_or_else(|_| panic!("import index budget"));
        assert_eq!(imports.wildcards.len(), COUNT);
        let mut scratch = ImportBindingScratch::new(bindings.len(), &mut budget)
            .unwrap_or_else(|_| panic!("import selection budget"));
        let identity = FileDocumentIdentity {
            file_id: file.clone(),
            path: "Use.java".to_owned(),
            language: "java".to_owned(),
        };
        let file_symbol = native_file_symbol_id(&file);
        let receivers = crate::native_pipeline::generic_resolution::ReceiverSites::default();
        let context = FileResolutionContext {
            identity: &identity,
            file_symbol_id: &file_symbol,
            import_bindings: &imports,
            current_receivers: &receivers,
        };
        let mut work = 0_usize;
        let mut cancelled = || {
            work += 1;
            work > COUNT * 20
        };
        prepare(
            &mut ResolutionIndexTarget {
                index: &mut index,
                budget: &mut budget,
            },
            &mut cancelled,
        )
        .unwrap_or_else(|_| panic!("wildcard preparation exceeded its work bound"));
        for number in 1..=COUNT {
            let reference = ExtractedReference {
                owner: None,
                name: format!("Missing{number}"),
                resolution_name: None,
                kind: ReferenceKind::TypeOf,
                span,
            };
            let lookup = ReferenceLookup::classify(&reference);
            let selection = select_import_bindings(
                (&context, &lookup, reference.kind, lookup.lookup_name),
                &mut scratch,
            );
            assert_eq!(selection.positions, [] as [usize; 0]);
            let resolution = resolve_extracted_reference(
                &index,
                ExtractedReferenceQuery {
                    context: &context,
                    reference: &reference,
                    import_binding_scratch: &mut scratch,
                },
                &mut cancelled,
            )
            .unwrap_or_else(|_| panic!("wildcard lookup exceeded its work bound"));
            assert_eq!(scratch.positions, [] as [usize; 0]);
            assert!(resolution.target.is_none());
            assert_eq!(resolution.unresolved_provenance, UNRESOLVED_PROVENANCE);
        }
        assert!(work <= COUNT * 20, "bounded cancellation probes: {work}");
    }

    fn wildcard_bindings(count: usize, span: SourceSpan) -> Vec<ExtractedImportBinding> {
        (1..=count)
            .map(|number| ExtractedImportBinding {
                kind: ImportBindingKind::Namespace,
                module_specifier: format!("external.p{number}"),
                imported_name: "*".to_owned(),
                local_name: "*".to_owned(),
                span,
            })
            .collect()
    }
}
