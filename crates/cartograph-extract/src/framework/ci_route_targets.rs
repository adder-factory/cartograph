//! Bounded `CodeIgniter` target normalization, excluding literal argument tails.

const MAX_ROUTE_SEGMENTS: usize = 16;
const SIMPLE_ROUTE_SEGMENTS: usize = 2;

pub(super) fn resolution(handler: &str) -> Option<String> {
    let mut segments = Vec::new();
    for part in handler.split('/').filter(|part| !part.is_empty()) {
        if argument(part) {
            if segments.len() < SIMPLE_ROUTE_SEGMENTS {
                return None;
            }
            break;
        }
        if segments.len() == MAX_ROUTE_SEGMENTS || !identifier(part) {
            return None;
        }
        segments.push(part);
    }
    // Directory segments and string arguments cannot be distinguished from
    // this literal alone. Defer those splits to declaration/path evidence.
    if segments.len() > SIMPLE_ROUTE_SEGMENTS {
        return Some(format!("ci-route-path::{}", segments.join("/")));
    }
    let controller = *segments.first()?;
    let method = segments.get(1).copied().unwrap_or("index");
    let mut class = controller.to_owned();
    let first = class.as_bytes().first()?.to_ascii_uppercase();
    class.replace_range(..1, &char::from(first).to_string());
    Some(format!("ci-route-root::{class}::{method}"))
}

fn argument(part: &str) -> bool {
    part.starts_with('$')
        || part.starts_with("(:")
        || part.bytes().all(|byte| byte.is_ascii_digit())
}

fn identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
