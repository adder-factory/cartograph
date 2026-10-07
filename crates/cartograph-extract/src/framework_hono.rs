mod details;
use std::collections::BTreeSet;

use cartograph_domain::SourceLanguage;

use crate::{
    ExtractError,
    framework::{
        DelimiterInput, FrameworkBuilder, FrameworkRouteInput,
        javascript_identifier_at as identifier_at, matching_delimiter, skip_ascii_whitespace,
    },
};

const MAX_RECEIVERS: usize = 256;
const MAX_ROUTES: usize = 4_096;
const MAX_ROUTE_BYTES: usize = 1_024;

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
    ) || !(source.contains("new Hono") || source.contains("new OpenAPIHono"))
    {
        return Ok(());
    }
    let receivers = collect_receivers(builder, source)?;
    if receivers.is_empty() {
        return Ok(());
    }
    let mut routes = Vec::new();
    routes
        .try_reserve_exact(MAX_ROUTES)
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut mounts = Vec::new();
    mounts
        .try_reserve_exact(MAX_RECEIVERS)
        .map_err(|_| ExtractError::OutputLimit)?;
    for receiver in &receivers {
        scan_receiver_calls(
            builder,
            ReceiverScan {
                source,
                receiver,
                routes: &mut routes,
                mounts: &mut mounts,
            },
        )?;
    }
    for route in &routes {
        builder.add_route(FrameworkRouteInput {
            method: &route.method,
            path: route.path,
            start: route.path_start,
            end: route.path_end,
            command: false,
            handler: route.handler,
        })?;
    }
    for mount in mounts {
        for route in &routes {
            if route.receiver != mount.child {
                continue;
            }
            let Some(path) = details::join_paths(mount.prefix, route.path) else {
                continue;
            };
            builder.add_route(FrameworkRouteInput {
                method: &route.method,
                path: &path,
                start: route.path_start,
                end: route.path_end,
                command: false,
                handler: route.handler,
            })?;
        }
    }
    Ok(())
}

fn collect_receivers<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &'source str,
) -> Result<BTreeSet<&'source str>, ExtractError> {
    let mut receivers = BTreeSet::new();
    for marker in ["new Hono", "new OpenAPIHono"] {
        let mut cursor = 0_usize;
        while receivers.len() < MAX_RECEIVERS
            && let Some(relative) = source[cursor..].find(marker)
        {
            builder.check_cancelled()?;
            let constructor = cursor + relative;
            if !details::constructor_is_syntax(builder, (constructor, constructor + marker.len())) {
                cursor = constructor + marker.len();
                continue;
            }
            let boundary = source[..constructor]
                .rfind(['\n', ';'])
                .map_or(0, |offset| offset + 1);
            let statement = &source[boundary..constructor];
            let Some(equals) = statement.rfind('=') else {
                cursor = constructor + marker.len();
                continue;
            };
            let declaration = statement[..equals].trim();
            if let Some(receiver) = details::receiver(declaration) {
                receivers.insert(receiver);
            }
            cursor = constructor + marker.len();
        }
    }
    Ok(receivers)
}

fn scan_receiver_calls(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ReceiverScan<'_, '_>,
) -> Result<(), ExtractError> {
    let ReceiverScan {
        source,
        receiver,
        routes,
        mounts,
    } = input;
    let marker = format!("{receiver}.");
    let mut cursor = 0_usize;
    while let Some(relative) = source[cursor..].find(&marker) {
        builder.check_cancelled()?;
        let call_start = cursor + relative;
        if call_start > 0 && identifier_byte(source.as_bytes()[call_start - 1]) {
            cursor = call_start + marker.len();
            continue;
        }
        let method_start = call_start + marker.len();
        let Some((method_end, method)) = identifier_at(source, method_start) else {
            cursor = method_start;
            continue;
        };
        if !crate::framework::member_call_is_syntax(builder, (call_start, method_end)) {
            cursor = method_end;
            continue;
        }
        let open = skip_ascii_whitespace(source, method_end);
        if source.as_bytes().get(open) != Some(&b'(') {
            cursor = method_end;
            continue;
        }
        let Some(close) = matching_delimiter(DelimiterInput::parentheses(source, open)) else {
            cursor = open + 1;
            continue;
        };
        builder.bridge.charge_work(close.saturating_sub(open))?;
        scan_hono_invocation(
            ReceiverScan {
                source,
                receiver,
                routes,
                mounts,
            },
            Invocation {
                method,
                open,
                close,
            },
        )?;
        cursor = open + 1;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Invocation<'a> {
    method: &'a str,
    open: usize,
    close: usize,
}

fn scan_hono_invocation(
    input: ReceiverScan<'_, '_>,
    invocation: Invocation<'_>,
) -> Result<(), ExtractError> {
    let Invocation {
        method,
        open,
        close,
    } = invocation;
    if direct_method(method) {
        return scan_direct_invocation(input, invocation);
    }
    match method {
        "on" => scan_on_invocation(input, open, close),
        "route" => scan_mount_invocation(input, open, close),
        _ => Ok(()),
    }
}

fn scan_direct_invocation(
    input: ReceiverScan<'_, '_>,
    invocation: Invocation<'_>,
) -> Result<(), ExtractError> {
    let Invocation {
        method,
        open,
        close,
    } = invocation;
    let ReceiverScan {
        source,
        receiver,
        routes,
        mounts: _,
    } = input;
    let Some(path) = quoted_argument(source, open + 1, close) else {
        return Ok(());
    };
    push_route(
        routes,
        HonoRoute {
            receiver,
            method: method.to_ascii_uppercase(),
            path: path.value,
            path_start: path.start,
            path_end: path.end,
            handler: handler_after_argument(source, path.quote_end + 1, close),
        },
    )
}

fn scan_on_invocation(
    input: ReceiverScan<'_, '_>,
    open: usize,
    close: usize,
) -> Result<(), ExtractError> {
    let ReceiverScan {
        source,
        receiver,
        routes,
        mounts: _,
    } = input;
    let Some(method) = quoted_argument(source, open + 1, close) else {
        return Ok(());
    };
    let Some(comma) = next_top_level_comma(source, method.quote_end + 1, close) else {
        return Ok(());
    };
    let Some(path) = quoted_argument(source, comma + 1, close) else {
        return Ok(());
    };
    push_route(
        routes,
        HonoRoute {
            receiver,
            method: method.value.to_ascii_uppercase(),
            path: path.value,
            path_start: path.start,
            path_end: path.end,
            handler: handler_after_argument(source, path.quote_end + 1, close),
        },
    )
}

fn scan_mount_invocation(
    input: ReceiverScan<'_, '_>,
    open: usize,
    close: usize,
) -> Result<(), ExtractError> {
    let ReceiverScan {
        source,
        receiver: _,
        routes: _,
        mounts,
    } = input;
    let Some(prefix) = quoted_argument(source, open + 1, close) else {
        return Ok(());
    };
    let Some(comma) = next_top_level_comma(source, prefix.quote_end + 1, close) else {
        return Ok(());
    };
    let child_start = skip_ascii_whitespace(source, comma + 1);
    let Some((_, child)) = identifier_at(source, child_start) else {
        return Ok(());
    };
    if mounts.len() >= MAX_RECEIVERS {
        return Err(ExtractError::OutputLimit);
    }
    mounts.push(HonoMount {
        prefix: prefix.value,
        child,
    });
    Ok(())
}

struct ReceiverScan<'a, 'source> {
    source: &'source str,
    receiver: &'source str,
    routes: &'a mut Vec<HonoRoute<'source>>,
    mounts: &'a mut Vec<HonoMount<'source>>,
}

fn push_route<'source>(
    routes: &mut Vec<HonoRoute<'source>>,
    route: HonoRoute<'source>,
) -> Result<(), ExtractError> {
    if routes.len() >= MAX_ROUTES || route.path.len() > MAX_ROUTE_BYTES {
        return Err(ExtractError::OutputLimit);
    }
    routes.push(route);
    Ok(())
}

struct HonoRoute<'source> {
    receiver: &'source str,
    method: String,
    path: &'source str,
    path_start: usize,
    path_end: usize,
    handler: Option<(&'source str, usize, usize)>,
}

struct HonoMount<'source> {
    prefix: &'source str,
    child: &'source str,
}

fn direct_method(method: &str) -> bool {
    matches!(
        method,
        "get" | "post" | "put" | "patch" | "delete" | "options" | "all"
    )
}

fn handler_after_argument(source: &str, from: usize, close: usize) -> Option<(&str, usize, usize)> {
    let comma = next_top_level_comma(source, from, close)?;
    let start = skip_ascii_whitespace(source, comma + 1);
    let (end, handler) = identifier_at(source, start)?;
    (!matches!(handler, "async" | "function")).then_some((handler, start, end))
}

fn next_top_level_comma(source: &str, start: usize, end: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut depth = DelimiterDepth::default();
    let mut quote = None;
    let mut escaped = false;
    while cursor < end {
        let byte = bytes[cursor];
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
            cursor += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
        } else if depth.advance(byte) {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

#[derive(Default)]
struct DelimiterDepth {
    paren: usize,
    brace: usize,
    bracket: usize,
}

impl DelimiterDepth {
    fn advance(&mut self, byte: u8) -> bool {
        match byte {
            b'(' => self.paren = self.paren.saturating_add(1),
            b')' => self.paren = self.paren.saturating_sub(1),
            b'{' => self.brace = self.brace.saturating_add(1),
            b'}' => self.brace = self.brace.saturating_sub(1),
            b'[' => self.bracket = self.bracket.saturating_add(1),
            b']' => self.bracket = self.bracket.saturating_sub(1),
            b',' => return self.paren == 0 && self.brace == 0 && self.bracket == 0,
            _ => {}
        }
        false
    }
}

struct Quoted<'source> {
    value: &'source str,
    start: usize,
    end: usize,
    quote_end: usize,
}

fn quoted_argument(source: &str, from: usize, limit: usize) -> Option<Quoted<'_>> {
    let quote_start = skip_ascii_whitespace(source, from);
    let quote = *source.as_bytes().get(quote_start)?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    let start = quote_start + 1;
    let mut cursor = start;
    let mut escaped = false;
    while cursor < limit {
        let byte = source.as_bytes()[cursor];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return Some(Quoted {
                value: &source[start..cursor],
                start,
                end: cursor,
                quote_end: cursor,
            });
        }
        cursor += 1;
    }
    None
}

fn identifiers(value: &str) -> impl Iterator<Item = (usize, &str)> {
    let bytes = value.as_bytes();
    let mut cursor = 0_usize;
    std::iter::from_fn(move || {
        while cursor < bytes.len()
            && !(bytes[cursor] == b'_'
                || bytes[cursor] == b'$'
                || bytes[cursor].is_ascii_alphabetic())
        {
            cursor += 1;
        }
        let start = cursor;
        let (end, name) = identifier_at(value, start)?;
        cursor = end;
        Some((start, name))
    })
}

fn identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphanumeric()
}
