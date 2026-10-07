//! Syntax-backed Bun registration with bounded configuration scanning.
use super::{ScanRange, scan_route_entries, top_level_routes_object};
use crate::{
    ExtractError,
    framework::{
        DelimiterInput, FrameworkBuilder, matching_delimiter, member_call_is_syntax,
        skip_ascii_whitespace,
    },
};

const CALLEE: &str = "Bun.serve";

pub(super) fn scan_call(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    call: usize,
) -> Result<usize, ExtractError> {
    let callee_end = call + CALLEE.len();
    if !member_call_is_syntax(builder, (call, callee_end)) {
        return Ok(callee_end);
    }
    let open = skip_ascii_whitespace(source, callee_end);
    if source.as_bytes().get(open) != Some(&b'(') {
        return Ok(callee_end);
    }
    let Some(close) = matching_delimiter(DelimiterInput::parentheses(source, open)) else {
        return Ok(open + 1);
    };
    builder.bridge.charge_work(close.saturating_sub(open))?;
    if let Some(range) = routes_range(source, (open, close)) {
        scan_route_entries(builder, source, range)?;
    }
    Ok(open + 1)
}

fn routes_range(source: &str, (open, close): (usize, usize)) -> Option<ScanRange> {
    let config_open = skip_ascii_whitespace(source, open + 1);
    if source.as_bytes().get(config_open) != Some(&b'{') {
        return None;
    }
    let config_close = matching_delimiter(DelimiterInput::braces(source, config_open))?;
    if config_close > close {
        return None;
    }
    let routes_open = top_level_routes_object(source, config_open + 1, config_close)?;
    let routes_close = matching_delimiter(DelimiterInput::braces(source, routes_open))?;
    (routes_close <= config_close).then_some(ScanRange {
        start: routes_open + 1,
        end: routes_close,
    })
}
