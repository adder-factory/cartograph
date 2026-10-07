//! Drupal tag hubs are deterministic within one Drupal installation root.

use std::collections::HashMap;

use super::{
    DRUPAL_TAG_EVIDENCE_PROVENANCE, DrupalServiceQuery, FRAMEWORK_CONVENTION_CONFIDENCE,
    FrameworkCandidateMutation, FrameworkEdgeInput, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    ResolutionCandidate, ResolutionIndex, ResolutionIndexTarget, StageItemFailure,
    append_framework_edge, drupal_tag_role, select_candidate, unique_drupal_service, usize_to_u64,
};

#[derive(Default)]
pub(super) struct TagIndex {
    hubs: HashMap<String, HashMap<String, Hub>>,
}

struct Hub {
    path: String,
    key: Option<String>,
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "yaml"
        || !super::drupal_resolution::services_path(&file.file.normalized_path)
    {
        return Ok(());
    }
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some((_, tag)) = symbol.input.qualified_name.split_once("::service-tag:") else {
            continue;
        };
        let root = root(&file.file.normalized_path);
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(
                    size_of::<Hub>()
                        + root.len()
                        + tag.len()
                        + file.file.normalized_path.len()
                        + symbol.input.qualified_name.len(),
                ),
        )?;
        let hub = target
            .index
            .frameworks
            .drupal_tags
            .hubs
            .entry(root.to_owned())
            .or_default()
            .entry(tag.to_owned())
            .or_insert_with(|| Hub {
                path: file.file.normalized_path.clone(),
                key: Some(symbol.input.qualified_name.clone()),
            });
        if hub.path > file.file.normalized_path {
            hub.path.clone_from(&file.file.normalized_path);
            hub.key = Some(symbol.input.qualified_name.clone());
        } else if hub.path == file.file.normalized_path
            && hub.key.as_ref() != Some(&symbol.input.qualified_name)
        {
            hub.key = None;
        }
    }
    Ok(())
}

pub(super) fn root(path: &str) -> &str {
    for directory in ["modules/", "profiles/", "themes/"] {
        if path.starts_with(directory) {
            return "";
        }
        if let Some((root, _)) = path.split_once(&format!("/{directory}")) {
            return root;
        }
    }
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

#[derive(Clone, Copy)]
pub(super) struct HubQuery<'query> {
    pub(super) index: &'query ResolutionIndex,
    pub(super) path: &'query str,
    pub(super) tag: &'query str,
}

pub(super) fn hub<'index, Cancel>(
    query: HubQuery<'index>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(key) = query
        .index
        .frameworks
        .drupal_tags
        .hubs
        .get(root(query.path))
        .and_then(|tags| tags.get(query.tag))
        .and_then(|hub| hub.key.as_deref())
    else {
        return Ok(None);
    };
    let Some(candidates) = query.index.candidates.get(key) else {
        return Ok(None);
    };
    select_candidate(
        candidates.iter(),
        |candidate| candidate.qualified_name == key,
        cancelled,
    )
}

pub(super) fn append<Cancel>(
    input: FrameworkCandidateMutation<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let FrameworkCandidateMutation {
        index,
        facts,
        budget,
        candidates,
        cancelled,
    } = input;
    for fact in candidates {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some((_, service_id)) = drupal_tag_role(fact) else {
            continue;
        };
        let file = index
            .modules
            .files
            .get(&fact.file_id)
            .ok_or(StageItemFailure)?;
        let Some((_, suffix)) = fact.qualified_name.split_once("::drupal-tag-") else {
            continue;
        };
        let Some(tag) = suffix.split("::").nth(1) else {
            continue;
        };
        let Some(hub) = hub(
            HubQuery {
                index,
                path: &file.path,
                tag,
            },
            cancelled,
        )?
        else {
            continue;
        };
        let Some(_) = unique_drupal_service(DrupalServiceQuery {
            index,
            file_id: &fact.file_id,
            service_id,
            cancelled,
        })?
        else {
            continue;
        };
        append_framework_edge(
            facts,
            budget,
            FrameworkEdgeInput {
                source: fact,
                target: hub,
                confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
                provenance: DRUPAL_TAG_EVIDENCE_PROVENANCE,
            },
        )?;
    }
    Ok(())
}
