use cartograph_domain::{SourceLanguage, SymbolKind};

use crate::{
    ExtractError,
    framework::{
        FrameworkBuilder, FrameworkRouteInput, LandmarkInput, Quoted, quoted_literal_after,
        segments::Segments, skip_ascii_whitespace,
    },
    source_lines::physical_lines,
};

const MAX_ROUTE_BYTES: usize = 1_024;

/// Screen source literals before scoped-path or controller normalization.
fn quoted_after(value: &str, from: usize) -> Option<Quoted<'_>> {
    quoted_literal_after(value, from).filter(|quoted| {
        !crate::walk::specifier_safety::specifier_may_carry_credential(quoted.value)
    })
}
const MAX_SCOPE_DEPTH: usize = 32;

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Ruby
        || !builder.path().to_ascii_lowercase().ends_with("routes.rb")
    {
        return Ok(());
    }
    let mut scopes = Vec::new();
    scopes
        .try_reserve_exact(MAX_SCOPE_DEPTH)
        .map_err(|_| ExtractError::OutputLimit)?;
    for (line_start, line) in physical_lines(source) {
        builder.bridge.charge_work(line.len())?;
        for (offset, statement) in Segments::new(line, b';') {
            builder.check_cancelled()?;
            scan_route_line(
                builder,
                MutableRouteLine {
                    line_start: line_start + offset,
                    line: statement,
                    scopes: &mut scopes,
                },
            )?;
        }
    }
    Ok(())
}

struct MutableRouteLine<'line, 'scope> {
    line_start: usize,
    line: &'line str,
    scopes: &'scope mut Vec<RouteScope>,
}

fn scan_route_line(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: MutableRouteLine<'_, '_>,
) -> Result<(), ExtractError> {
    let MutableRouteLine {
        line_start,
        line,
        scopes,
    } = input;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if trimmed == "end" {
        scopes.pop();
        return Ok(());
    }
    if let Some(scope) = parse_scope(trimmed) {
        if scopes.len() >= MAX_SCOPE_DEPTH {
            return Err(ExtractError::NestingLimit);
        }
        scopes.push(scope);
        return Ok(());
    }
    let input = RouteLineInput {
        line_start,
        line,
        scopes,
    };
    if trimmed.starts_with("root") {
        return scan_root(builder, input);
    }
    if trimmed.starts_with("resources ") || trimmed.starts_with("resource ") {
        return scan_resource(builder, input);
    }
    scan_http_route(builder, input)
}

#[derive(Clone)]
struct RouteScope {
    path: String,
    module: String,
}

#[derive(Clone, Copy)]
struct RouteLineInput<'line, 'scope> {
    line_start: usize,
    line: &'line str,
    scopes: &'scope [RouteScope],
}

fn parse_scope(line: &str) -> Option<RouteScope> {
    if let Some(rest) = line.strip_prefix("namespace ") {
        let name = ruby_symbol_or_string(rest)?;
        return Some(RouteScope {
            path: name.to_owned(),
            module: camelize(name),
        });
    }
    if let Some(rest) = line.strip_prefix("scope ") {
        let path = ruby_symbol_or_string(rest)?;
        return Some(RouteScope {
            path: path.trim_matches('/').to_owned(),
            module: String::new(),
        });
    }
    None
}

fn scan_root(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteLineInput<'_, '_>,
) -> Result<(), ExtractError> {
    let RouteLineInput {
        line_start,
        line,
        scopes,
    } = input;
    let Some(target) = quoted_after(line, line.find("root").unwrap_or(0) + "root".len()) else {
        return Ok(());
    };
    let path = scoped_path(scopes, "/")?;
    let resolution = controller_action_identity(target.value, scopes);
    let action_offset = target.value.rfind('#').map_or(0, |offset| offset + 1);
    let action = &target.value[action_offset..];
    let target_start = line_start + target.start + action_offset;
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Route,
        name: format!("{path} -> {}", target.value),
        identity: format!("rails-root::{path}::{}", target.value),
        start: line_start + target.start,
        end: line_start + target.end,
        body_search_text: format!("rails root route {path} {}", target.value),
        target: (!action.is_empty()).then_some((
            action,
            resolution.as_deref(),
            target_start,
            target_start + action.len(),
        )),
    })
}

fn scan_resource(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteLineInput<'_, '_>,
) -> Result<(), ExtractError> {
    let RouteLineInput {
        line_start,
        line,
        scopes,
    } = input;
    let trimmed = line.trim_start();
    let singular = trimmed.starts_with("resource ") && !trimmed.starts_with("resources ");
    let keyword = if singular { "resource" } else { "resources" };
    let keyword_start = line.find(keyword).unwrap_or(0);
    let rest_start = keyword_start + keyword.len();
    let Some((resource, start, end)) = ruby_symbol_or_string_with_span(line, rest_start) else {
        return Ok(());
    };
    let base = scoped_path(scopes, &format!("/{resource}"))?;
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Route,
        name: format!("resource:{resource}"),
        identity: format!("rails-resource::{base}"),
        start: line_start + start,
        end: line_start + end,
        body_search_text: format!("rails resource route {resource} {base}"),
        target: None,
    })?;
    let resource_routes: &[(&str, &str)] = if singular {
        &[
            ("GET", ""),
            ("GET", "/new"),
            ("POST", ""),
            ("GET", "/edit"),
            ("PATCH", ""),
            ("DELETE", ""),
        ]
    } else {
        &[
            ("GET", ""),
            ("GET", "/new"),
            ("POST", ""),
            ("GET", "/:id"),
            ("GET", "/:id/edit"),
            ("PATCH", "/:id"),
            ("DELETE", "/:id"),
        ]
    };
    for (method, suffix) in resource_routes {
        builder.add_route(FrameworkRouteInput {
            method,
            path: &format!("{base}{suffix}"),
            start: line_start + start,
            end: line_start + end,
            command: false,
            handler: None,
        })?;
    }
    Ok(())
}

fn scan_http_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteLineInput<'_, '_>,
) -> Result<(), ExtractError> {
    let RouteLineInput {
        line_start,
        line,
        scopes,
    } = input;
    let trimmed = line.trim_start();
    let indentation = line.len().saturating_sub(trimmed.len());
    let Some((keyword, method)) = [
        ("get", "GET"),
        ("post", "POST"),
        ("put", "PUT"),
        ("patch", "PATCH"),
        ("delete", "DELETE"),
    ]
    .into_iter()
    .find(|(keyword, _)| {
        trimmed.starts_with(keyword)
            && trimmed
                .as_bytes()
                .get(keyword.len())
                .is_some_and(u8::is_ascii_whitespace)
    }) else {
        return Ok(());
    };
    let Some(path) = quoted_after(trimmed, keyword.len()) else {
        return Ok(());
    };
    let full_path = scoped_path(scopes, path.value)?;
    let handler = find_route_target(trimmed, path.quote_end + 1);
    let resolution = handler
        .as_ref()
        .and_then(|handler| controller_action_identity(handler.value, scopes));
    let target = handler.as_ref().and_then(|handler| {
        let action_offset = handler.value.rfind('#').map_or(0, |offset| offset + 1);
        let action = &handler.value[action_offset..];
        (!action.is_empty()).then_some((
            action,
            resolution.as_deref(),
            line_start + indentation + handler.start + action_offset,
            line_start + indentation + handler.start + action_offset + action.len(),
        ))
    });
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Route,
        name: format!("{method} {full_path}"),
        identity: format!("rails-http::{method}::{full_path}"),
        start: line_start + indentation + path.start,
        end: line_start + indentation + path.end,
        body_search_text: format!("rails http route {method} {full_path}"),
        target,
    })
}

fn find_route_target(line: &str, from: usize) -> Option<Quoted<'_>> {
    let suffix = &line[from..];
    let marker = suffix.find("to:").map(|offset| from + offset + "to:".len());
    marker
        .and_then(|start| quoted_after(line, start))
        .or_else(|| quoted_after(line, from))
        .filter(|quoted| quoted.value.contains('#'))
}

fn controller_action_identity(target: &str, scopes: &[RouteScope]) -> Option<String> {
    let (controller, action) = target.split_once('#')?;
    if controller.is_empty() || action.is_empty() {
        return None;
    }
    let mut qualified = String::new();
    let required = scopes
        .iter()
        .map(|scope| scope.module.len())
        .sum::<usize>()
        .saturating_add(target.len())
        .saturating_add(scopes.len().saturating_mul(2))
        .saturating_add("Controller::".len());
    qualified.try_reserve(required).ok()?;
    for scope in scopes {
        if scope.module.is_empty() {
            continue;
        }
        if !qualified.is_empty() {
            qualified.push_str("::");
        }
        qualified.push_str(&scope.module);
    }
    for segment in controller.split('/') {
        if !qualified.is_empty() {
            qualified.push_str("::");
        }
        qualified.push_str(&camelize(segment));
    }
    qualified.push_str("Controller::");
    qualified.push_str(action);
    Some(qualified)
}

fn scoped_path(scopes: &[RouteScope], path: &str) -> Result<String, ExtractError> {
    let required = scopes
        .iter()
        .map(|scope| scope.path.len())
        .sum::<usize>()
        .saturating_add(path.len())
        .saturating_add(scopes.len().saturating_add(1));
    if required > MAX_ROUTE_BYTES {
        return Err(ExtractError::OutputLimit);
    }
    let mut output = String::new();
    output
        .try_reserve(required)
        .map_err(|_| ExtractError::OutputLimit)?;
    for segment in scopes
        .iter()
        .flat_map(|scope| scope.path.split('/'))
        .chain(path.split('/'))
    {
        if segment.is_empty() {
            continue;
        }
        output.push('/');
        output.push_str(segment);
    }
    if output.is_empty() {
        output.push('/');
    }
    Ok(output)
}

fn camelize(value: &str) -> String {
    let mut output = String::new();
    let mut uppercase = true;
    for byte in value.bytes() {
        if matches!(byte, b'_' | b'-') {
            uppercase = true;
        } else if uppercase {
            output.push(char::from(byte.to_ascii_uppercase()));
            uppercase = false;
        } else {
            output.push(char::from(byte));
        }
    }
    output
}

fn ruby_symbol_or_string(value: &str) -> Option<&str> {
    ruby_symbol_or_string_with_span(value, 0).map(|(value, _, _)| value)
}

fn ruby_symbol_or_string_with_span(value: &str, from: usize) -> Option<(&str, usize, usize)> {
    let start = skip_ascii_whitespace(value, from);
    if value.as_bytes().get(start) == Some(&b':') {
        let name_start = start + 1;
        let end = identifier_end(value, name_start);
        return (end > name_start).then_some((&value[name_start..end], name_start, end));
    }
    quoted_after(value, start).map(|quoted| (quoted.value, quoted.start, quoted.end))
}

fn identifier_end(value: &str, mut cursor: usize) -> usize {
    while value
        .as_bytes()
        .get(cursor)
        .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
    {
        cursor += 1;
    }
    cursor
}
