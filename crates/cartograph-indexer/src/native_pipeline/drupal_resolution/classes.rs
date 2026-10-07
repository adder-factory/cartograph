//! Index scalar class intent by occurrence, avoiding repeated same-name scans.

use std::collections::{HashMap, HashSet};

use cartograph_extract::DRUPAL_CLASS_MODULE;

use super::super::{
    FileId, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndexTarget,
    ResolutionRequest, SourceSpan, StageItemFailure, usize_to_u64,
};

#[derive(Default)]
pub(in crate::native_pipeline) struct ClassIndex {
    sites: HashMap<FileId, HashSet<SourceSpan>>,
}

impl ClassIndex {
    pub(super) fn contains(&self, request: &ResolutionRequest<'_>) -> bool {
        self.sites
            .get(request.file_id)
            .is_some_and(|sites| sites.contains(&request.span))
    }
}

pub(in crate::native_pipeline) fn index_file<Cancel>(
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
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if binding.module_specifier != DRUPAL_CLASS_MODULE {
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<FileId>() + file.file.file_id.as_str().len())
                + usize_to_u64(size_of::<HashSet<SourceSpan>>() + size_of::<SourceSpan>()),
        )?;
        target
            .index
            .drupal_classes
            .sites
            .entry(file.file.file_id.clone())
            .or_default()
            .insert(binding.span);
    }
    Ok(())
}
