//! Framework file routes retain directory segments and exact declaration sites.
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, LandmarkInput},
};
use cartograph_domain::SymbolKind;

pub(crate) fn scan(builder: &mut FrameworkBuilder<'_, '_>) -> Result<(), ExtractError> {
    let path = builder.path().replace('\\', "/");
    let next_span = default_export_span(builder)?;
    let next = next_route(&path, builder.source().is_empty() || next_span.is_some());
    if let Some(route) = sveltekit_route(&path)
        .or_else(|| nuxt_route(&path))
        .or_else(|| next.clone())
    {
        let (start, end) = if next.is_some() {
            next_span.unwrap_or_else(|| convention_span(builder.source()))
        } else {
            convention_span(builder.source())
        };
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Route,
            name: route.clone(),
            identity: format!("route::{route}"),
            start,
            end,
            body_search_text: format!("framework file route {route}"),
            target: None,
        })?;
    }
    if let Some(name) = nuxt_middleware_name(&path) {
        if has_named_function(builder, &name)? {
            return Ok(());
        }
        let (start, end) = convention_span(builder.source());
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Function,
            name: name.clone(),
            identity: format!("middleware::{name}"),
            start,
            end,
            body_search_text: format!("nuxt middleware {name}"),
            target: None,
        })?;
    }
    Ok(())
}

fn has_named_function(
    builder: &mut FrameworkBuilder<'_, '_>,
    name: &str,
) -> Result<bool, ExtractError> {
    for position in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        if builder.original_symbol(position).is_some_and(|symbol| {
            matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) && symbol.name == name
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn convention_span(source: &str) -> (usize, usize) {
    let start = source
        .char_indices()
        .find_map(|(index, character)| (!character.is_whitespace()).then_some(index))
        .unwrap_or(0);
    let end = source
        .get(start..)
        .and_then(|tail| tail.chars().next())
        .map_or(start, |character| {
            start.saturating_add(character.len_utf8())
        });
    (start, end)
}

fn sveltekit_route(path: &str) -> Option<String> {
    let marker = "/routes/";
    let marker_start = format!("/{path}").find(marker)?;
    let normalized = format!("/{path}");
    let after_routes = &normalized[marker_start + marker.len()..];
    let (directory, file_name) = after_routes.rsplit_once('/').unwrap_or(("", after_routes));
    if !matches!(
        file_name,
        "+page.svelte"
            | "+page.ts"
            | "+page.js"
            | "+page.server.ts"
            | "+page.server.js"
            | "+layout.svelte"
            | "+layout.ts"
            | "+layout.js"
            | "+layout.server.ts"
            | "+layout.server.js"
            | "+server.ts"
            | "+server.js"
            | "+error.svelte"
    ) {
        return None;
    }
    Some(route_from_segments(directory.split('/'), false))
}

fn nuxt_route(path: &str) -> Option<String> {
    let normalized = format!("/{path}");
    if let Some(index) = normalized.find("/server/api/") {
        let remainder = &normalized[index + "/server/api/".len()..];
        if !matches!(
            file_extension(remainder),
            Some("ts" | "js" | "mts" | "mjs" | "cjs")
        ) {
            return None;
        }
        let stem = strip_final_extension(remainder);
        let route = route_from_segments(stem.split('/'), true);
        return Some(if route == "/" {
            "/api".to_owned()
        } else {
            format!("/api{route}")
        });
    }
    let index = normalized.find("/pages/")?;
    let remainder = &normalized[index + "/pages/".len()..];
    if file_extension(remainder) != Some("vue") {
        return None;
    }
    Some(route_from_segments(
        strip_final_extension(remainder).split('/'),
        true,
    ))
}

fn nuxt_middleware_name(path: &str) -> Option<String> {
    let normalized = format!("/{path}");
    let index = normalized.find("/middleware/")?;
    let remainder = &normalized[index + "/middleware/".len()..];
    if !matches!(
        file_extension(remainder),
        Some("ts" | "js" | "mts" | "mjs" | "cjs")
    ) {
        return None;
    }
    let stem = strip_final_extension(remainder).trim_end_matches("/index");
    (!stem.is_empty()).then(|| stem.replace('/', "."))
}

fn next_route(path: &str, default_export: bool) -> Option<String> {
    if !matches!(
        file_extension(path),
        Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs")
    ) {
        return None;
    }
    let normalized = format!("/{path}");
    if let Some(index) = normalized.find("/pages/") {
        let remainder = &normalized[index + "/pages/".len()..];
        let stem = strip_final_extension(remainder);
        let basename = stem.rsplit('/').next().unwrap_or(stem);
        if basename.starts_with('_') || !default_export {
            return None;
        }
        return Some(route_from_segments(stem.split('/'), true));
    }
    let index = normalized.find("/app/")?;
    let remainder = &normalized[index + "/app/".len()..];
    let (directory, filename) = remainder.rsplit_once('/').unwrap_or(("", remainder));
    if !(filename.starts_with("page.") || filename.starts_with("route."))
        || (filename.starts_with("page.") && !default_export)
    {
        return None;
    }
    Some(route_from_segments(
        directory.split('/').filter(|segment| {
            !(segment.starts_with('@') || segment.starts_with('(') && segment.ends_with(')'))
        }),
        false,
    ))
}

fn strip_final_extension(value: &str) -> &str {
    value.rsplit_once('.').map_or(value, |(stem, _)| stem)
}

fn file_extension(value: &str) -> Option<&str> {
    value.rsplit_once('.').map(|(_, extension)| extension)
}

fn route_from_segments<'segment>(
    segments: impl IntoIterator<Item = &'segment str>,
    drop_terminal_index: bool,
) -> String {
    let mut route = String::new();
    let mut segments = segments.into_iter().peekable();
    while let Some(raw) = segments.next() {
        if raw.is_empty() || drop_terminal_index && raw == "index" && segments.peek().is_none() {
            continue;
        }
        route.push('/');
        route.push_str(&route_segment(raw));
    }
    if route.is_empty() {
        route.push('/');
    }
    route
}

fn route_segment(raw: &str) -> String {
    if let Some(name) = raw
        .strip_circumfix("[[", "]]")
        .filter(|name| !name.is_empty())
    {
        return format!(":{name}?");
    }
    if let Some(name) = raw
        .strip_circumfix("[...", ']')
        .filter(|name| !name.is_empty())
    {
        return format!("*{name}");
    }
    let mut segment = String::new();
    let mut remainder = raw;
    while let Some(open) = remainder.find('[') {
        let Some(close) = remainder[open + 1..].find(']') else {
            break;
        };
        let end = open + 1 + close;
        if end == open + 1 {
            break;
        }
        segment.push_str(&remainder[..open]);
        segment.push(':');
        segment.push_str(&remainder[open + 1..end]);
        remainder = &remainder[end + 1..];
    }
    segment.push_str(remainder);
    segment
}

fn default_export_span(
    builder: &mut FrameworkBuilder<'_, '_>,
) -> Result<Option<(usize, usize)>, ExtractError> {
    if !builder.source().contains("export") {
        return Ok(None);
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(None);
    };
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if node.kind() != "export_statement" {
            continue;
        }
        let mut children = node.walk();
        for child in node.children(&mut children) {
            builder.bridge.charge_work(1)?;
            if child.kind() == "default" {
                return Ok(Some((node.start_byte(), child.end_byte())));
            }
        }
    }
    Ok(None)
}
