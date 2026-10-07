//! Retain the package written in an existing mapper-class reference span.

use cartograph_domain::{ReferenceKind, SourceLanguage};

use super::{FrameworkBuilder, FrameworkReferenceInput};
use crate::{ExtractError, ExtractedReference};

const MAX_CLASS_BYTES: usize = 1_024;
const UNPROVEN_CLASS_LOOKUP: &str = "cartograph.mybatis-unproven-class";

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Xml
        || !source.contains("<configuration")
        || !source.contains("<mappers")
    {
        return Ok(());
    }
    let count = builder.references().len();
    for index in 0..count {
        builder.bridge.charge_work(1)?;
        let reference = &builder.references()[index];
        let Some((start, end, raw)) = class_span(source, reference) else {
            continue;
        };
        let Some(resolution) = class_lookup(raw, &reference.name) else {
            continue;
        };
        let name = reference.name.clone();
        builder.bridge.charge_work(raw.len().min(MAX_CLASS_BYTES))?;
        builder.add_reference(FrameworkReferenceInput {
            owner: None,
            name: &name,
            resolution_name: Some(&resolution),
            kind: ReferenceKind::References,
            start,
            end,
        })?;
    }
    Ok(())
}

fn class_span<'s>(
    source: &'s str,
    reference: &ExtractedReference,
) -> Option<(usize, usize, &'s str)> {
    if reference.owner.is_some() || reference.kind != ReferenceKind::References {
        return None;
    }
    let start = usize::try_from(reference.span.start_byte()).ok()?;
    let end = usize::try_from(reference.span.end_byte()).ok()?;
    Some((start, end, source.get(start..end)?))
}

fn class_lookup(raw: &str, name: &str) -> Option<String> {
    if raw.len() > MAX_CLASS_BYTES {
        return Some(UNPROVEN_CLASS_LOOKUP.to_owned());
    }
    let (package, class) = raw.rsplit_once('.')?;
    (class == name).then(|| format!("mybatis-class::{package}::{class}"))
}
