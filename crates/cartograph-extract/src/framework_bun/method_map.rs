//! Top-level HTTP method properties may use literal or identifier keys.
use super::{
    METHODS, MethodMapInput, ScanRange, direct_handler, identifier_at, quoted_at,
    route_delimiter_depth, skip_ascii_whitespace,
};
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, FrameworkRouteInput, LandmarkInput, safe_route_value},
};
use cartograph_domain::SymbolKind;

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    input: MethodMapInput<'_, '_>,
) -> Result<(), ExtractError> {
    let MethodMapInput {
        path,
        range: ScanRange { start, end },
    } = input;
    let mut cursor = start;
    let mut depth = 0_usize;
    while cursor < end {
        builder.bridge.charge_work(1)?;
        let byte = source.as_bytes()[cursor];
        if let Some(next) = route_delimiter_depth(byte, depth) {
            depth = next;
            cursor += 1;
            continue;
        }
        let Some((next, key)) = property_key(source, cursor, end) else {
            cursor += 1;
            continue;
        };
        let colon = skip_ascii_whitespace(source, next);
        if depth == 0 && METHODS.contains(&key) && source.as_bytes().get(colon) == Some(&b':') {
            let input = FrameworkRouteInput {
                method: key,
                path: path.value,
                start: path.start,
                end: path.end,
                command: false,
                handler: direct_handler(source, colon + 1, end),
            };
            if matches!(byte, b'\'' | b'"' | b'`') {
                add_literal_method(builder, input)?;
            } else {
                builder.add_route(input)?;
            }
        }
        cursor = next;
    }
    Ok(())
}

/// Newly admitted literal method keys use a Bun identity so they cannot
/// renumber earlier Hono/Express landmarks with the same method and path.
fn add_literal_method(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: FrameworkRouteInput<'_>,
) -> Result<(), ExtractError> {
    let Some(path) = safe_route_value(input.path, false) else {
        return Ok(());
    };
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Route,
        name: format!("{} {path}", input.method),
        identity: format!("bun::{}::{path}", input.method.to_ascii_lowercase()),
        start: input.start,
        end: input.end,
        body_search_text: format!("route {} {path}", input.method),
        target: input
            .handler
            .map(|(name, start, end)| (name, None, start, end)),
    })
}

fn property_key(source: &str, cursor: usize, end: usize) -> Option<(usize, &str)> {
    if matches!(source.as_bytes()[cursor], b'\'' | b'"' | b'`') {
        let quoted = quoted_at(source, cursor, end)?;
        return Some((quoted.quote_end + 1, quoted.value));
    }
    identifier_at(source, cursor)
}
