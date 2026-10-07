use super::{first_argument, literal, text};
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, LandmarkInput, safe_route_value},
};
use cartograph_domain::SymbolKind;
use tree_sitter::Node;

pub(super) fn scan_call(
    builder: &mut FrameworkBuilder<'_, '_>,
    call: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(());
    };
    let Some(property) = function.child_by_field_name("property") else {
        return Ok(());
    };
    if !matches!(text(builder, property), "command" | "command_name") {
        return Ok(());
    }
    let Some(argument) = first_argument(call) else {
        return Ok(());
    };
    let Some(spec) = literal(builder, argument).map(str::trim) else {
        return Ok(());
    };
    let Some(name) = safe_route_value(spec, true).filter(|name| command_name(name)) else {
        return Ok(());
    };
    if !spec.bytes().all(spec_byte) {
        return Ok(());
    }
    builder.add_signed_landmark(
        LandmarkInput {
            kind: SymbolKind::Route,
            name: format!("cmd {name}"),
            identity: format!("cmd::{name}"),
            start: argument.start_byte() + 1,
            end: argument.end_byte() - 1,
            body_search_text: format!("cli command {name}"),
            target: None,
        },
        spec.to_owned(),
    )?;
    Ok(())
}

fn command_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn spec_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b' ' | b'_' | b'-' | b'[' | b']' | b'<' | b'>' | b'.' | b'|'
        )
}
