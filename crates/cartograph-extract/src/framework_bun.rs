mod method_map;
mod registration;
use cartograph_domain::SourceLanguage;

use crate::{
    ExtractError,
    framework::{
        DelimiterInput, FrameworkBuilder, FrameworkRouteInput,
        javascript_identifier_at as identifier_at, matching_delimiter, skip_ascii_whitespace,
    },
};

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if !matches!(
        builder.language(),
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    ) || !source.contains("Bun.serve")
    {
        return Ok(());
    }
    let mut cursor = 0_usize;
    while let Some(relative) = source[cursor..].find("Bun.serve") {
        builder.check_cancelled()?;
        let call = cursor + relative;
        cursor = registration::scan_call(builder, source, call)?;
    }
    Ok(())
}

fn top_level_routes_object(source: &str, start: usize, end: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    while cursor < end {
        let byte = bytes[cursor];
        if advance_route_quote(byte, &mut quote, &mut escaped) {
            cursor += 1;
            continue;
        }
        if let Some(next_depth) = route_delimiter_depth(byte, depth) {
            depth = next_depth;
            cursor += 1;
            continue;
        }
        if depth != 0 || !(byte == b'_' || byte.is_ascii_alphabetic()) {
            cursor += 1;
            continue;
        }
        let (next, routes) = top_level_routes_property(source, cursor);
        if routes.is_some() {
            return routes;
        }
        cursor = next;
    }
    None
}

fn top_level_routes_property(source: &str, cursor: usize) -> (usize, Option<usize>) {
    let Some((name_end, name)) = identifier_at(source, cursor) else {
        return (cursor.saturating_add(1), None);
    };
    let colon = skip_ascii_whitespace(source, name_end);
    if name != "routes" || source.as_bytes().get(colon) != Some(&b':') {
        return (name_end, None);
    }
    let value = skip_ascii_whitespace(source, colon.saturating_add(1));
    let routes = (source.as_bytes().get(value) == Some(&b'{')).then_some(value);
    (name_end, routes)
}

fn advance_route_quote(byte: u8, quote: &mut Option<u8>, escaped: &mut bool) -> bool {
    if let Some(active_quote) = *quote {
        if *escaped {
            *escaped = false;
        } else if byte == b'\\' {
            *escaped = true;
        } else if byte == active_quote {
            *quote = None;
        }
        return true;
    }
    if matches!(byte, b'\'' | b'"' | b'`') {
        *quote = Some(byte);
        return true;
    }
    false
}

const fn route_delimiter_depth(byte: u8, depth: usize) -> Option<usize> {
    match byte {
        b'{' | b'[' | b'(' => Some(depth.saturating_add(1)),
        b'}' | b']' | b')' => Some(depth.saturating_sub(1)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct ScanRange {
    start: usize,
    end: usize,
}

fn scan_route_entries(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    range: ScanRange,
) -> Result<(), ExtractError> {
    let ScanRange { start, end } = range;
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut depth = 0_usize;
    while cursor < end {
        builder.check_cancelled()?;
        let byte = bytes[cursor];
        if let Some(next_depth) = route_delimiter_depth(byte, depth) {
            depth = next_depth;
            cursor += 1;
            continue;
        }
        if depth != 0 || !matches!(byte, b'\'' | b'"' | b'`') {
            cursor += 1;
            continue;
        }
        let Some(next) = scan_route_entry(
            builder,
            RouteEntryInput {
                source,
                cursor,
                end,
            },
        )?
        else {
            return Ok(());
        };
        cursor = next;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RouteEntryInput<'a> {
    source: &'a str,
    cursor: usize,
    end: usize,
}

fn scan_route_entry(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteEntryInput<'_>,
) -> Result<Option<usize>, ExtractError> {
    let RouteEntryInput {
        source,
        cursor,
        end,
    } = input;
    let Some(path) = quoted_at(source, cursor, end) else {
        return Ok(None);
    };
    let colon = skip_ascii_whitespace(source, path.quote_end.saturating_add(1));
    if !path.value.starts_with('/') || source.as_bytes().get(colon) != Some(&b':') {
        return Ok(Some(path.quote_end.saturating_add(1)));
    }
    let value = skip_ascii_whitespace(source, colon.saturating_add(1));
    if source.as_bytes().get(value) != Some(&b'{') {
        let handler = direct_handler(source, value, end);
        builder.add_route(FrameworkRouteInput {
            method: "ANY",
            path: path.value,
            start: path.start,
            end: path.end,
            command: false,
            handler,
        })?;
        return Ok(Some(value.saturating_add(1)));
    }
    let Some(map_close) = matching_delimiter(DelimiterInput::braces(source, value)) else {
        return Ok(None);
    };
    if map_close > end {
        return Ok(None);
    }
    method_map::scan(
        builder,
        source,
        MethodMapInput {
            path: &path,
            range: ScanRange {
                start: value.saturating_add(1),
                end: map_close,
            },
        },
    )?;
    Ok(Some(map_close.saturating_add(1)))
}

#[derive(Clone, Copy)]
struct MethodMapInput<'path, 'source> {
    path: &'path Quoted<'source>,
    range: ScanRange,
}

fn direct_handler(source: &str, start: usize, limit: usize) -> Option<(&str, usize, usize)> {
    let start = skip_ascii_whitespace(source, start);
    let (end, name) = identifier_at(source, start)?;
    (!matches!(name, "async" | "function" | "new") && end <= limit).then_some((name, start, end))
}

struct Quoted<'source> {
    value: &'source str,
    start: usize,
    end: usize,
    quote_end: usize,
}

fn quoted_at(value: &str, quote_start: usize, limit: usize) -> Option<Quoted<'_>> {
    let quote = *value.as_bytes().get(quote_start)?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    let start = quote_start + 1;
    let mut cursor = start;
    let mut escaped = false;
    while cursor < limit {
        let byte = value.as_bytes()[cursor];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return Some(Quoted {
                value: &value[start..cursor],
                start,
                end: cursor,
                quote_end: cursor,
            });
        }
        cursor += 1;
    }
    None
}
