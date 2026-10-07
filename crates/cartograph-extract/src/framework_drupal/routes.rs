//! One Drupal route per literal routing mapping; handlers are references.

use super::{
    ExtractError, FrameworkBuilder, FrameworkReferenceInput, LandmarkInput, MAX_SIGNAL_BYTES,
    ReferenceKind, SymbolKind, physical_lines, service_details::flow_items, skip_ascii_whitespace,
    yaml_first_key, yaml_mapping_key, yaml_value_for_key,
};
use crate::framework::safe_route_value;

const HANDLER_KEYS: [&str; 6] = [
    "_controller",
    "_form",
    "_entity_form",
    "_entity_list",
    "_entity_view",
    "controller",
];
const MAX_METHODS: usize = 16;

pub(crate) fn is_routing_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".routing.yml") || lower.ends_with(".routing.yaml")
}

struct Handler {
    key: &'static str,
    name: String,
    start: usize,
    end: usize,
}

struct Route {
    key: String,
    start: usize,
    end: usize,
    path: Option<String>,
    methods: Option<String>,
    handlers: Vec<Handler>,
    member_indent: Option<usize>,
    defaults_indent: Option<usize>,
    handler_indent: Option<usize>,
    handler_keys: u8,
    valid: bool,
}

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let mut route = None;
    for (start, line) in physical_lines(source) {
        builder.bridge.charge_work(line.len())?;
        let Some((key, key_start, key_end)) = yaml_mapping_key(line) else {
            continue;
        };
        if line.len() == line.trim_start().len() {
            publish(builder, route.take())?;
            if key.len() > MAX_SIGNAL_BYTES {
                continue;
            }
            builder.bridge.reserve_working_bytes(
                u64::try_from(size_of::<Route>() + key.len())
                    .map_err(|_| ExtractError::OutputLimit)?,
            )?;
            route = Some(Route {
                key: key.to_owned(),
                start: start + key_start,
                end: start + key_end,
                path: None,
                methods: None,
                handlers: Vec::new(),
                member_indent: None,
                defaults_indent: None,
                handler_indent: None,
                handler_keys: 0,
                valid: true,
            });
        } else if let Some(route) = &mut route {
            capture(builder, route, (start, line, key))?;
        }
    }
    publish(builder, route)
}

fn capture(
    builder: &mut FrameworkBuilder<'_, '_>,
    route: &mut Route,
    line: (usize, &str, &str),
) -> Result<(), ExtractError> {
    let (start, text, key) = line;
    let indent = text.len() - text.trim_start().len();
    if indent == *route.member_indent.get_or_insert(indent) {
        route.defaults_indent = (key == "defaults").then_some(indent);
        route.handler_indent = None;
        capture_member(builder, route, line)?;
    } else if route.defaults_indent.is_some_and(|depth| indent > depth)
        && indent == *route.handler_indent.get_or_insert(indent)
    {
        capture_handler(builder, route, (start, text, key))?;
    }
    Ok(())
}

fn capture_member(
    builder: &mut FrameworkBuilder<'_, '_>,
    route: &mut Route,
    line: (usize, &str, &str),
) -> Result<(), ExtractError> {
    let (start, text, key) = line;
    if key == "path" {
        route.valid &= route.path.is_none();
        route.path =
            yaml_value_for_key(text, key).and_then(|value| safe_route_value(&value.value, false));
    } else if key == "methods" {
        route.methods = methods(text);
    } else if key == "defaults" {
        flow_defaults(builder, route, (start, text))?;
    } else if key == "controller" {
        capture_handler(builder, route, (start, text, key))?;
    }
    Ok(())
}

fn flow_defaults(
    builder: &mut FrameworkBuilder<'_, '_>,
    route: &mut Route,
    line: (usize, &str),
) -> Result<(), ExtractError> {
    let (start, text) = line;
    let Some((_, colon, _)) = yaml_first_key(text) else {
        return Ok(());
    };
    let value_start = skip_ascii_whitespace(text, colon + 1);
    let Some(body) = text[value_start..]
        .trim_end()
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
    else {
        return Ok(());
    };
    let Some(items) = flow_items(body) else {
        return Ok(());
    };
    for (offset, item) in items {
        builder.bridge.charge_work(item.len())?;
        if let Some((key, _, _)) = yaml_first_key(item) {
            capture_handler(
                builder,
                route,
                (start + value_start + 1 + offset, item, key),
            )?;
        }
    }
    Ok(())
}

fn capture_handler(
    builder: &mut FrameworkBuilder<'_, '_>,
    route: &mut Route,
    line: (usize, &str, &str),
) -> Result<(), ExtractError> {
    let (start, text, key) = line;
    let Some(key_index) = HANDLER_KEYS.iter().position(|candidate| *candidate == key) else {
        return Ok(());
    };
    let mask = 1 << key_index;
    if route.handler_keys & mask != 0 {
        route.valid = false;
        return Ok(());
    }
    route.handler_keys |= mask;
    let Some(value) =
        yaml_value_for_key(text, key).filter(|value| value.value.len() <= MAX_SIGNAL_BYTES)
    else {
        return Ok(());
    };
    builder.bridge.reserve_working_bytes(
        u64::try_from(size_of::<Handler>() + value.value.len())
            .map_err(|_| ExtractError::OutputLimit)?,
    )?;
    route.handlers.push(Handler {
        key: HANDLER_KEYS[key_index],
        name: value.value.into_owned(),
        start: start + value.start,
        end: start + value.end,
    });
    Ok(())
}

fn methods(line: &str) -> Option<String> {
    let (_, value) = line.split_once(':')?;
    let body = value.trim().strip_prefix('[')?.strip_suffix(']')?;
    let items = flow_items(body)?;
    if items.len() > MAX_METHODS {
        return None;
    }
    let mut names = Vec::new();
    for (_, item) in items {
        let name = item.trim().trim_matches(['\'', '"']);
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return None;
        }
        names.push(name.to_ascii_uppercase());
    }
    Some(names.join(","))
}

fn publish(
    builder: &mut FrameworkBuilder<'_, '_>,
    route: Option<Route>,
) -> Result<(), ExtractError> {
    let Some(route) = route else {
        return Ok(());
    };
    let Some(path) = route.path.filter(|_| route.valid) else {
        return Ok(());
    };
    let name = route
        .methods
        .as_ref()
        .map_or_else(|| path.clone(), |methods| format!("{path} [{methods}]"));
    let Some(owner) = builder.add_landmark_with_id(LandmarkInput {
        kind: SymbolKind::Route,
        name,
        identity: route.key,
        start: route.start,
        end: route.end,
        body_search_text: format!(
            "drupal route {path} {}",
            route.methods.as_deref().unwrap_or("ANY")
        ),
        target: None,
    })?
    else {
        return Ok(());
    };
    for handler in route.handlers {
        builder.check_cancelled()?;
        let resolution = handler_resolution(&handler);
        builder.add_reference(FrameworkReferenceInput {
            owner: Some(owner.clone()),
            name: &handler.name,
            resolution_name: resolution.as_deref(),
            kind: if handler.key == "controller" {
                ReferenceKind::Calls
            } else {
                ReferenceKind::References
            },
            start: handler.start,
            end: handler.end,
        })?;
    }
    Ok(())
}

fn handler_resolution(handler: &Handler) -> Option<String> {
    match handler.key {
        "_entity_form" | "_entity_list" | "_entity_view" => None,
        "controller" => crate::framework::php_controller_resolution(&handler.name),
        _ => super::service_details::class_lookup(&handler.name)
            .or_else(|| crate::framework::php_controller_resolution(&handler.name)),
    }
}
