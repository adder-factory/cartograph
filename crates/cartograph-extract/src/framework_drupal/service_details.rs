//! Literal shorthand, flow mappings and two-element service factories.

use super::{
    ExtractError, FrameworkBuilder, FrameworkReferenceInput, ReferenceKind, ServiceLine,
    ServiceReference, ServiceSourceLine, ServiceState, YamlScalar, scan_service_direct_references,
    skip_ascii_whitespace, yaml_first_key, yaml_scalar_at,
};
use crate::framework::consume_quoted_byte;

const MAX_FLOW_ITEMS: usize = 64;
const MAX_FLOW_BYTES: usize = 4_096;
const MAX_CLASS_BYTES: usize = 512;

pub(super) fn declaration(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (Option<&ServiceState>, ServiceSourceLine<'_>),
) -> Result<(), ExtractError> {
    let (Some(service), line) = input else {
        return Ok(());
    };
    let Some((_, colon, _)) = yaml_first_key(line.text) else {
        return Ok(());
    };
    let value_start = skip_ascii_whitespace(line.text, colon + 1);
    let value = &line.text[value_start..];
    if service.id.contains('\\') && matches!(value.trim(), "~" | "null") {
        let start = line.start + line.text.find(&service.id).unwrap_or_default();
        add_class_reference(
            builder,
            ServiceReference {
                service,
                value: &service.id,
                start,
                end: start + service.id.len(),
                class_target: true,
            },
        )?;
    }
    let Some(body) = value
        .trim_end()
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
    else {
        return Ok(());
    };
    let Some(items) = flow_items(body) else {
        return Ok(());
    };
    for (offset, text) in items {
        builder.check_cancelled()?;
        scan_service_direct_references(
            builder,
            ServiceLine {
                service,
                start: line.start + value_start + 1 + offset,
                text,
                service_setting: true,
            },
        )?;
    }
    Ok(())
}

pub(super) fn factory(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ServiceLine<'_>,
) -> Result<(), ExtractError> {
    if !input.service_setting {
        return Ok(());
    }
    let Some(("factory", colon, _)) = yaml_first_key(input.text) else {
        return Ok(());
    };
    let start = skip_ascii_whitespace(input.text, colon + 1);
    let Some(class) = sequence_factory(input.text, start) else {
        return Ok(());
    };
    if class.value.starts_with('@') {
        return Ok(());
    }
    add_class_reference(
        builder,
        ServiceReference {
            service: input.service,
            value: &class.value,
            start: input.start + class.start,
            end: input.start + class.end,
            class_target: true,
        },
    )
}

fn add_class_reference(
    builder: &mut FrameworkBuilder<'_, '_>,
    reference: ServiceReference<'_>,
) -> Result<(), ExtractError> {
    let Some(resolution) = class_lookup(reference.value) else {
        return Ok(());
    };
    builder.add_reference(FrameworkReferenceInput {
        owner: Some(reference.service.symbol_id.clone()),
        name: reference.value,
        resolution_name: Some(&resolution),
        kind: ReferenceKind::References,
        start: reference.start,
        end: reference.end,
    })
}

pub(super) fn class_lookup(value: &str) -> Option<String> {
    let value = value.trim_start_matches('\\');
    if value.is_empty()
        || value.len() > MAX_CLASS_BYTES
        || !value.split('\\').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
        })
    {
        return None;
    }
    let key = value.rsplit_once('\\').map_or_else(
        || value.to_owned(),
        |(namespace, class)| format!("{namespace}::{class}"),
    );
    Some(format!(
        "{}class::{key}",
        crate::PHP_EXACT_RESOLUTION_PREFIX
    ))
}

fn sequence_factory(line: &str, start: usize) -> Option<YamlScalar<'_>> {
    let body = line[start..]
        .strip_prefix('[')?
        .trim_end()
        .strip_suffix(']')?;
    let items = flow_items(body)?;
    let [class, method] = items.as_slice() else {
        return None;
    };
    let mut class = sequence_operand(*class)?;
    let method = sequence_operand(*method)?;
    if !method
        .value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    class.start += start + 1;
    class.end += start + 1;
    Some(class)
}

fn sequence_operand(input: (usize, &str)) -> Option<YamlScalar<'_>> {
    let (offset, text) = input;
    let start = skip_ascii_whitespace(text, 0);
    let mut scalar = yaml_scalar_at(text, start, false)?;
    let quoted = matches!(text.as_bytes().get(start), Some(b'\'' | b'"'));
    let end = scalar.end + usize::from(quoted);
    if !text[end..].trim().is_empty() {
        return None;
    }
    scalar.start += offset;
    scalar.end += offset;
    Some(scalar)
}

/// A bounded, complete list split outside nested containers and quoted scalars.
pub(super) fn flow_items(body: &str) -> Option<Vec<(usize, &str)>> {
    if body.len() > MAX_FLOW_BYTES {
        return None;
    }
    let mut items = Vec::new();
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, byte) in body.bytes().enumerate() {
        if consume_quoted_byte(byte, &mut quote, &mut escaped) {
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'[' | b'{' => depth = depth.checked_add(1)?,
            b']' | b'}' => depth = depth.checked_sub(1)?,
            b',' if depth == 0 => {
                items.push((start, &body[start..index]));
                start = index + 1;
            }
            _ => {}
        }
        if items.len() >= MAX_FLOW_ITEMS {
            return None;
        }
    }
    if quote.is_some() || depth != 0 {
        return None;
    }
    items.push((start, &body[start..]));
    Some(items)
}
