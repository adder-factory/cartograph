use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind, Visibility};

use crate::{
    ExtractError,
    framework::{
        DelimiterInput, FrameworkBuilder, FrameworkNearReferenceInput, FrameworkRouteInput,
        matching_delimiter, skip_ascii_whitespace,
    },
};

const MAX_LOADED_RESOURCES: usize = 256;
const MAX_SCAN_BYTES: usize = 4_096;

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Php {
        return Ok(());
    }
    let path = builder.path().to_ascii_lowercase();
    if path.starts_with("application/controllers/") {
        scan_controller_routes(builder, source)?;
        scan_loaded_resources(builder, source)?;
    }
    Ok(())
}

fn scan_controller_routes(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let relative = builder
        .path()
        .strip_prefix("application/controllers/")
        .or_else(|| {
            let lower = builder.path().to_ascii_lowercase();
            lower
                .find("application/controllers/")
                .map(|offset| &builder.path()[offset + "application/controllers/".len()..])
        })
        .unwrap_or_default();
    let controller = relative
        .rsplit_once('.')
        .map_or(relative, |(stem, _)| stem)
        .trim_matches('/');
    if controller.is_empty() {
        return Ok(());
    }
    let route_base = format!("/{}", controller.to_ascii_lowercase());
    for index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
        let Some((name, start, end, visibility)) =
            builder.original_symbol(index).and_then(|symbol| {
                (symbol.kind == SymbolKind::Method).then(|| {
                    Some((
                        symbol.name.clone(),
                        usize::try_from(symbol.span.start_byte()).ok()?,
                        usize::try_from(symbol.span.end_byte()).ok()?,
                        symbol.visibility,
                    ))
                })?
            })
        else {
            continue;
        };
        if name.starts_with('_')
            || matches!(name.as_str(), "__construct" | "initialize")
            || matches!(
                visibility,
                Some(Visibility::Private | Visibility::Protected)
            )
        {
            continue;
        }
        let route = if name == "index" {
            route_base.clone()
        } else {
            format!("{route_base}/{name}")
        };
        let (name_start, name_end) = source[start..end]
            .find(&name)
            .map_or((start, end), |offset| {
                (start + offset, start + offset + name.len())
            });
        builder.add_route(FrameworkRouteInput {
            method: "ANY",
            path: &route,
            start: name_start,
            end: name_end,
            command: false,
            handler: Some((&name, name_start, name_end)),
        })?;
    }
    Ok(())
}

fn scan_loaded_resources(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let mut resources = Vec::new();
    resources
        .try_reserve_exact(MAX_LOADED_RESOURCES)
        .map_err(|_| ExtractError::OutputLimit)?;
    for marker in ["$this->load->model(", "$this->load->library("] {
        let mut cursor = 0_usize;
        while resources.len() < MAX_LOADED_RESOURCES
            && let Some(relative) = source[cursor..].find(marker)
        {
            builder.check_cancelled()?;
            let call = cursor + relative;
            let Some(close) = matching_delimiter(DelimiterInput::bounded_parentheses(
                source,
                call + marker.len() - 1,
                MAX_SCAN_BYTES,
            )) else {
                cursor = call + marker.len();
                continue;
            };
            let Some(resource) = quoted_after(source, call + marker.len(), close) else {
                cursor = close + 1;
                continue;
            };
            let alias = next_quoted_after_comma(source, resource.quote_end + 1, close);
            let Some(loaded) = screened_loaded_resource(&resource, alias) else {
                cursor = close + 1;
                continue;
            };
            builder.add_reference_near_with_resolution(FrameworkNearReferenceInput {
                name: resource.value,
                resolution_name: Some(&loaded.class),
                kind: ReferenceKind::References,
                start: resource.start,
                end: resource.end,
            })?;
            resources.push(loaded);
            cursor = close + 1;
        }
    }
    for resource in resources {
        let marker = format!("$this->{}->", resource.alias);
        let mut cursor = 0_usize;
        while let Some(relative) = source[cursor..].find(&marker) {
            builder.check_cancelled()?;
            let method_start = cursor + relative + marker.len();
            let Some((method_end, method)) = identifier_at(source, method_start) else {
                cursor = method_start;
                continue;
            };
            let open = skip_ascii_whitespace(source, method_end);
            if source.as_bytes().get(open) == Some(&b'(') {
                builder.add_reference_near_with_resolution(FrameworkNearReferenceInput {
                    name: method,
                    resolution_name: Some(&format!("{}::{method}", resource.class)),
                    kind: ReferenceKind::Calls,
                    start: method_start,
                    end: method_end,
                })?;
            }
            cursor = method_end;
        }
    }
    Ok(())
}

struct LoadedResource {
    alias: String,
    class: String,
}

/// Screen source operands before basename/capitalization and alias registration.
fn screened_loaded_resource(
    resource: &Quoted<'_>,
    alias: Option<Quoted<'_>>,
) -> Option<LoadedResource> {
    if resource.unsupported_escape
        || crate::walk::specifier_safety::specifier_may_carry_credential(resource.value)
        || alias.as_ref().is_some_and(|alias| {
            alias.unsupported_escape
                || crate::walk::specifier_safety::specifier_may_carry_credential(alias.value)
        })
    {
        return None;
    }
    let alias = alias.map_or_else(
        || {
            resource
                .value
                .rsplit('/')
                .next()
                .unwrap_or(resource.value)
                .to_owned()
        },
        |quoted| quoted.value.to_owned(),
    );
    Some(LoadedResource {
        alias,
        class: ci_class_name(resource.value),
    })
}

fn ci_class_name(resource: &str) -> String {
    let base = resource.rsplit('/').next().unwrap_or(resource);
    let mut class = base.to_owned();
    if let Some(first) = class.as_bytes().first() {
        class.replace_range(..1, &char::from(first.to_ascii_uppercase()).to_string());
    }
    class
}

fn next_quoted_after_comma(value: &str, from: usize, limit: usize) -> Option<Quoted<'_>> {
    let comma = value[from..limit].find(',')? + from + 1;
    quoted_after(value, comma, limit)
}

struct Quoted<'source> {
    value: &'source str,
    start: usize,
    end: usize,
    quote_end: usize,
    unsupported_escape: bool,
}

fn quoted_after(value: &str, from: usize, limit: usize) -> Option<Quoted<'_>> {
    let mut cursor = from;
    while cursor < limit && !matches!(value.as_bytes()[cursor], b'\'' | b'"') {
        cursor += 1;
    }
    let quote = *value.as_bytes().get(cursor)?;
    let start = cursor + 1;
    cursor = start;
    let mut escaped = false;
    let mut unsupported_escape = false;
    while cursor < limit {
        let byte = value.as_bytes()[cursor];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
            unsupported_escape |= quote == b'"'
                || value
                    .as_bytes()
                    .get(cursor + 1)
                    .is_some_and(|next| matches!(next, b'\\' | b'\''));
        } else if byte == quote {
            return Some(Quoted {
                value: &value[start..cursor],
                start,
                end: cursor,
                quote_end: cursor,
                unsupported_escape,
            });
        }
        cursor += 1;
    }
    None
}

fn identifier_at(value: &str, start: usize) -> Option<(usize, &str)> {
    let first = *value.as_bytes().get(start)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = start + 1;
    while value
        .as_bytes()
        .get(end)
        .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
    {
        end += 1;
    }
    Some((end, &value[start..end]))
}
