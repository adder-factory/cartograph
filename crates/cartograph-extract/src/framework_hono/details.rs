//! Hono receivers are bare assignments; mount joining changes only the seam.
use super::{MAX_ROUTE_BYTES, identifiers};
use crate::framework::FrameworkBuilder;

pub(super) fn constructor_is_syntax(
    builder: &FrameworkBuilder<'_, '_>,
    (start, end): (usize, usize),
) -> bool {
    builder
        .syntax_root()
        .and_then(|root| root.descendant_for_byte_range(start, end))
        .is_some_and(|node| {
            node.kind() == "new_expression"
                && node.start_byte() == start
                && node
                    .child_by_field_name("constructor")
                    .is_some_and(|constructor| {
                        constructor.kind() == "identifier"
                            && matches!(
                                builder.source().get(constructor.byte_range()),
                                Some("Hono" | "OpenAPIHono")
                            )
                    })
        })
}

pub(super) fn receiver(declaration: &str) -> Option<&str> {
    if declaration.contains(['.', '[', ']', '{', '}']) {
        return None;
    }
    identifiers(declaration).last().map(|(_, name)| name)
}

pub(super) fn join_paths(prefix: &str, path: &str) -> Option<String> {
    let bytes = prefix.len().saturating_add(path.len()).saturating_add(1);
    if bytes > MAX_ROUTE_BYTES {
        return None;
    }
    let mut joined = String::new();
    joined.try_reserve(bytes).ok()?;
    joined.push_str(prefix.trim_end_matches('/'));
    joined.push('/');
    joined.push_str(path.trim_start_matches('/'));
    Some(joined)
}
