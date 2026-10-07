mod loads;
mod property_bindings;
mod resources;

use cartograph_domain::{SourceLanguage, SymbolKind, Visibility};

use crate::{
    ExtractError,
    framework::{FrameworkBuilder, FrameworkRouteInput},
};

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
    }
    if path.starts_with("application/") || source.contains("extends CI_") {
        let loaded = loads::scan(builder, source)?;
        resources::scan_calls(builder, source, &loaded)?;
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

struct LoadedResource {
    alias: String,
    class: String,
    path: String,
    kind: &'static str,
}

/// Screen source operands before basename/capitalization and alias registration.
fn screened_loaded_resource(
    resource: &Quoted<'_>,
    alias: Option<Quoted<'_>>,
    kind: &'static str,
) -> Option<LoadedResource> {
    if unsafe_resource_operand(resource) || alias.as_ref().is_some_and(unsafe_resource_operand) {
        return None;
    }
    let alias = alias.map_or_else(
        || {
            resource
                .value
                .rsplit('/')
                .next()
                .unwrap_or(resource.value)
                .to_ascii_lowercase()
        },
        |quoted| quoted.value.to_owned(),
    );
    let class = ci_class_name(resource.value);
    let path = resource.value.rsplit_once('/').map_or_else(
        || class.clone(),
        |(directory, _)| format!("{directory}/{class}"),
    );
    Some(LoadedResource {
        alias,
        class,
        path,
        kind,
    })
}

fn unsafe_resource_operand(quoted: &Quoted<'_>) -> bool {
    !quoted.value.is_ascii()
        || quoted.unsupported_escape
        || crate::walk::specifier_safety::specifier_may_carry_credential(quoted.value)
}

fn ci_class_name(resource: &str) -> String {
    let base = resource.rsplit('/').next().unwrap_or(resource);
    let mut class = base.to_owned();
    if let Some(first) = class.as_bytes().first() {
        class.replace_range(..1, &char::from(first.to_ascii_uppercase()).to_string());
    }
    class
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
