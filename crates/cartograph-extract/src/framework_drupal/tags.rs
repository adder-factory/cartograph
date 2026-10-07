//! Preserve per-service tag evidence and one shared tag declaration per file.

use std::collections::BTreeSet;

use super::{
    ExtractError, FrameworkBuilder, FrameworkReferenceInput, LandmarkInput, MAX_TAGS_PER_FILE,
    ReferenceKind, ServiceTagFact, SymbolKind,
};

const TAG_INDEX_ENTRY_BYTES: u64 = 256;

pub(super) fn publish(
    builder: &mut FrameworkBuilder<'_, '_>,
    facts: BTreeSet<ServiceTagFact>,
) -> Result<(), ExtractError> {
    if facts.len() > MAX_TAGS_PER_FILE {
        return Err(ExtractError::OutputLimit);
    }
    let mut published = BTreeSet::new();
    for (service_id, tag, provider, start, end, owner) in facts {
        builder.bridge.charge_work(1)?;
        if published.insert(tag.clone()) {
            builder
                .bridge
                .reserve_working_bytes(TAG_INDEX_ENTRY_BYTES)?;
            builder.add_landmark(LandmarkInput {
                kind: SymbolKind::Resource,
                name: tag.clone(),
                identity: format!("service-tag:{tag}"),
                start: 0,
                end: builder.source().chars().next().map_or(0, char::len_utf8),
                body_search_text: format!("drupal service tag {tag}"),
                target: None,
            })?;
        }
        let resolution = format!(
            "{}::drupal-service-tag-{}::{tag}",
            builder.path(),
            if provider { "provider" } else { "consumer" }
        );
        builder.add_reference(FrameworkReferenceInput {
            owner: Some(owner),
            name: &tag,
            resolution_name: Some(&resolution),
            kind: ReferenceKind::References,
            start,
            end,
        })?;
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Resource,
            name: format!("drupal-tag:{tag}"),
            identity: format!(
                "drupal-tag-{}::{tag}::{service_id}",
                if provider { "provider" } else { "consumer" }
            ),
            start,
            end,
            body_search_text: format!(
                "drupal service tag {} {tag} {service_id}",
                if provider { "provides" } else { "consumes" }
            ),
            target: None,
        })?;
    }
    Ok(())
}
