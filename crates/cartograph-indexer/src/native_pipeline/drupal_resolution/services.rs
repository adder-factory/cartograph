//! Resolve same-file services from one installation-scoped identity index.

use std::collections::HashMap;

use super::super::{
    FileId, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceResolution, ResolutionIndex,
    ResolutionIndexTarget, ResolutionRequest, StageItemFailure, SymbolId, SymbolKind, drupal_tags,
    usize_to_u64,
};

const SERVICE_PREFIX_SUFFIX: &str = "::drupal-service::";

#[derive(Default)]
pub(in crate::native_pipeline) struct ServiceIndex {
    roots: HashMap<String, HashMap<String, Option<ServiceTarget>>>,
}

struct ServiceTarget {
    file_id: FileId,
    symbol_id: SymbolId,
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "yaml" || !super::services_path(&file.file.normalized_path) {
        return Ok(());
    }
    let root = drupal_tags::root(&file.file.normalized_path);
    target.budget.charge(usize_to_u64(
        file.file.normalized_path.len() + SERVICE_PREFIX_SUFFIX.len(),
    ))?;
    let prefix = format!("{}{SERVICE_PREFIX_SUFFIX}", file.file.normalized_path);
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = symbol
            .input
            .qualified_name
            .strip_prefix(&prefix)
            .filter(|_| symbol.kind == SymbolKind::Resource)
        else {
            continue;
        };
        insert(
            target,
            &ServiceInsertion {
                root,
                id,
                file_id: &file.file.file_id,
                symbol_id: &symbol.input.symbol_id,
            },
        )?;
    }
    Ok(())
}

struct ServiceInsertion<'symbol> {
    root: &'symbol str,
    id: &'symbol str,
    file_id: &'symbol FileId,
    symbol_id: &'symbol SymbolId,
}

fn insert(
    target: &mut ResolutionIndexTarget<'_>,
    input: &ServiceInsertion<'_>,
) -> Result<(), StageItemFailure> {
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<ServiceTarget>()
                    + input.root.len()
                    + input.id.len()
                    + input.file_id.as_str().len()
                    + input.symbol_id.as_str().len(),
            ),
    )?;
    target
        .index
        .drupal_services
        .roots
        .entry(input.root.to_owned())
        .or_default()
        .entry(input.id.to_owned())
        .and_modify(|known| {
            if known
                .as_ref()
                .is_some_and(|known| &known.symbol_id != input.symbol_id)
            {
                *known = None;
            }
        })
        .or_insert_with(|| {
            Some(ServiceTarget {
                file_id: input.file_id.clone(),
                symbol_id: input.symbol_id.clone(),
            })
        });
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    let Some(service) = index
        .drupal_services
        .roots
        .get(drupal_tags::root(request.file_path))
        .and_then(|services| services.get(request.name))
        .and_then(Option::as_ref)
        .filter(|service| service.file_id == *request.file_id)
    else {
        // Cross-file references and ambiguous identities keep the base resolver,
        // including its exact target, confidence and provenance when available.
        return Ok(None);
    };
    Ok(Some(ReferenceResolution::resolved(super::target(
        (&service.symbol_id, SymbolKind::Resource),
        "framework-drupal-resource",
    ))))
}
