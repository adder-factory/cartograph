//! Preserve mapper namespace and declaration roles in XML template lookups.

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId};

use super::{FrameworkBuilder, FrameworkReferenceInput};
use crate::{ExtractError, ExtractedReference};

const MAX_TEMPLATE_BYTES: usize = 1_024;
const MAX_ATTRIBUTE_LOOKBEHIND: usize = 128;
const UNPROVEN_TEMPLATE_LOOKUP: &str = "cartograph.mybatis-unproven-template";

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Xml || !source.contains("<mapper") {
        return Ok(());
    }
    let count = builder.references().len();
    for index in 0..count {
        builder.bridge.charge_work(1 + MAX_ATTRIBUTE_LOOKBEHIND)?;
        let Some(reference) = template_reference(source, &builder.references()[index])? else {
            continue;
        };
        builder.bridge.charge_work(reference.work)?;
        builder.add_reference(FrameworkReferenceInput {
            owner: reference.owner,
            name: &reference.name,
            resolution_name: Some(&reference.lookup),
            kind: ReferenceKind::References,
            start: reference.start,
            end: reference.end,
        })?;
    }
    Ok(())
}

struct TemplateReference {
    owner: Option<SymbolId>,
    name: String,
    lookup: String,
    start: usize,
    end: usize,
    work: usize,
}

fn template_reference(
    source: &str,
    reference: &ExtractedReference,
) -> Result<Option<TemplateReference>, ExtractError> {
    let Some((start, end, raw)) = template_span(source, reference)? else {
        return Ok(None);
    };
    let lookup = attribute_at(source, start).map_or_else(
        || UNPROVEN_TEMPLATE_LOOKUP.to_owned(),
        |attribute| template_lookup((raw, &reference.name), attribute),
    );
    Ok(Some(TemplateReference {
        owner: reference.owner.clone(),
        name: reference.name.clone(),
        lookup,
        start,
        end,
        work: raw.len().min(MAX_TEMPLATE_BYTES),
    }))
}

fn template_span<'s>(
    source: &'s str,
    reference: &ExtractedReference,
) -> Result<Option<(usize, usize, &'s str)>, ExtractError> {
    if reference.owner.is_none()
        || reference.kind != ReferenceKind::References
        || !reference.name.contains("::")
    {
        return Ok(None);
    }
    let start =
        usize::try_from(reference.span.start_byte()).map_err(|_| ExtractError::OutputLimit)?;
    let end = usize::try_from(reference.span.end_byte()).map_err(|_| ExtractError::OutputLimit)?;
    if !source
        .as_bytes()
        .get(start.saturating_sub(1))
        .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
    {
        return Ok(None);
    }
    Ok(source.get(start..end).map(|raw| (start, end, raw)))
}

fn attribute_at(source: &str, start: usize) -> Option<(&str, &str)> {
    let mut begin = start.saturating_sub(MAX_ATTRIBUTE_LOOKBEHIND);
    while !source.is_char_boundary(begin) {
        begin += 1;
    }
    let prefix = source.get(begin..start)?;
    let before_quote = prefix.strip_suffix(['\'', '"'])?.trim_end();
    let before_equals = before_quote.strip_suffix('=')?.trim_end();
    let key_start = before_equals
        .rfind(|character: char| !character.is_ascii_alphabetic())
        .map_or(0, |offset| offset + 1);
    Some((&before_equals[key_start..], prefix))
}

fn template_lookup(input: (&str, &str), attribute: (&str, &str)) -> String {
    let (raw, source_name) = input;
    let Some(role) = template_role(attribute) else {
        return UNPROVEN_TEMPLATE_LOOKUP.to_owned();
    };
    if raw.len() > MAX_TEMPLATE_BYTES {
        return UNPROVEN_TEMPLATE_LOOKUP.to_owned();
    }
    if let Some((namespace, id)) = raw.rsplit_once('.') {
        return format!("mybatis-template::{namespace}::{id}::{role}");
    }
    format!("mybatis-template-local::{source_name}::{role}")
}

fn template_role((attribute, prefix): (&str, &str)) -> Option<&'static str> {
    match attribute {
        "resultMap" => Some("resultMap"),
        "parameterMap" => Some("parameterMap"),
        "refid" => Some("sql"),
        "extends" => {
            let tag = prefix.rsplit_once('<')?.1.split_ascii_whitespace().next()?;
            match tag {
                "resultMap" => Some("resultMap"),
                "parameterMap" => Some("parameterMap"),
                _ => None,
            }
        }
        _ => None,
    }
}
