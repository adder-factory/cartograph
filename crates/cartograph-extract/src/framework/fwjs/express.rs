use super::{first_argument, lexical, literal, text};
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, FrameworkResolvedRouteInput},
};
use tree_sitter::Node;

pub(super) fn scan_call(
    builder: &mut FrameworkBuilder<'_, '_>,
    call: Node<'_>,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(());
    };
    let Some(receiver) = function.child_by_field_name("object") else {
        return Ok(());
    };
    if receiver.kind() != "identifier" || !matches!(text(builder, receiver), "app" | "router") {
        return Ok(());
    }
    let Some(property) = function.child_by_field_name("property") else {
        return Ok(());
    };
    let method = text(builder, property);
    if !matches!(
        method,
        "get" | "post" | "put" | "patch" | "delete" | "all" | "use"
    ) {
        return Ok(());
    }
    let Some(argument) = first_argument(call) else {
        return Ok(());
    };
    let Some(path) = literal(builder, argument).filter(|path| path.starts_with('/')) else {
        return Ok(());
    };
    let handler = call
        .child_by_field_name("arguments")
        .and_then(|arguments| arguments.named_child(1))
        .filter(|node| node.kind() == "identifier");
    let resolution = match handler {
        Some(node) => lexical.resolution(builder, node)?,
        None => None,
    };
    builder.add_route_with_target(FrameworkResolvedRouteInput {
        method,
        path,
        start: argument.start_byte() + 1,
        end: argument.end_byte() - 1,
        command: false,
        target: handler.zip(resolution.as_ref()).map(|(node, resolution)| {
            (
                text(builder, node),
                resolution.resolution_name.as_deref(),
                node.start_byte(),
                node.end_byte(),
            )
        }),
    })
}
