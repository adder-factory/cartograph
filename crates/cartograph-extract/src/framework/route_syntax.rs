//! Literal route anchors suffice without an import in the same source file.
//! Tree nodes exclude comments and strings; unknown arguments remain unknown.

use cartograph_domain::SourceLanguage;
use tree_sitter::Node;

use super::{
    FrameworkBuilder, FrameworkRouteInput, Quoted, quoted_after, skip_ascii_whitespace,
    syntax_nodes::SyntaxNodes,
};
use crate::ExtractError;

const MAX_CALL_BYTES: usize = 4_096;
const HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
const MAX_ROUTE_METHODS: usize = HTTP_METHODS.len();

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if !source.contains('(') {
        return Ok(());
    }
    if !matches!(
        builder.language(),
        SourceLanguage::Python
            | SourceLanguage::Rust
            | SourceLanguage::Go
            | SourceLanguage::CSharp
            | SourceLanguage::Swift
    ) {
        return Ok(());
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(());
    };
    let vapor = builder.language() == SourceLanguage::Swift && source.contains("Vapor");
    for node in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        match (builder.language(), node.kind()) {
            (SourceLanguage::Python, "decorator") | (SourceLanguage::Rust, "attribute_item") => {
                scan_annotation(builder, (source, node))?;
            }
            (
                SourceLanguage::Go | SourceLanguage::Rust | SourceLanguage::Swift,
                "call_expression",
            )
            | (SourceLanguage::CSharp, "invocation_expression") => scan_call(
                builder,
                CallSite {
                    source,
                    node,
                    vapor,
                },
            )?,
            (SourceLanguage::Go, "composite_literal") => scan_cobra(builder, (source, node))?,
            _ => {}
        }
    }
    Ok(())
}

fn node_text<'s>(source: &'s str, node: Node<'_>) -> Option<&'s str> {
    source.get(node.byte_range())
}

fn scan_annotation(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, Node<'_>),
) -> Result<(), ExtractError> {
    let (source, node) = input;
    let Some(text) = node_text(source, node).filter(|text| text.len() <= MAX_CALL_BYTES) else {
        return Ok(());
    };
    builder.bridge.charge_work(text.len())?;
    let Some(open) = text.find('(') else {
        return Ok(());
    };
    let header = text[..open]
        .trim()
        .trim_start_matches("#[")
        .trim_start_matches('@');
    let token = header.rsplit('.').next().unwrap_or(header).trim();
    let method = if token == "route" && builder.language() == SourceLanguage::Python {
        "ANY".to_owned()
    } else {
        token.to_ascii_uppercase()
    };
    if method != "ANY" && !HTTP_METHODS.contains(&method.as_str()) {
        return Ok(());
    }
    if builder.language() == SourceLanguage::Python && !header.contains('.') {
        return Ok(());
    }
    let route = LiteralRoute {
        text,
        offset: node.start_byte(),
        open,
        method: &method,
    };
    if method == "ANY"
        && let Some(value) = method_argument(source, node)
    {
        return emit_flask_methods(builder, (source, value), route);
    }
    emit_literal(builder, route)
}

#[derive(Clone, Copy)]
struct LiteralRoute<'s> {
    text: &'s str,
    offset: usize,
    open: usize,
    method: &'s str,
}

fn emit_literal(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: LiteralRoute<'_>,
) -> Result<(), ExtractError> {
    let first = skip_ascii_whitespace(input.text, input.open + 1);
    let Some(path) = quoted_after(input.text, first)
        .filter(|path| path.start == first + 1 && complete_literal(input.text, path))
    else {
        return Ok(());
    };
    if !path.value.starts_with('/') {
        return Ok(());
    }
    builder.add_route(FrameworkRouteInput {
        method: input.method,
        path: path.value,
        start: input.offset + path.start,
        end: input.offset + path.end,
        command: false,
        handler: None,
    })
}

#[derive(Clone, Copy)]
struct CallSite<'s, 'tree> {
    source: &'s str,
    node: Node<'tree>,
    vapor: bool,
}

fn scan_call(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: CallSite<'_, '_>,
) -> Result<(), ExtractError> {
    let CallSite {
        source,
        node,
        vapor,
    } = input;
    let Some(text) =
        node_text(source, node).and_then(|text| text.get(..text.len().min(MAX_CALL_BYTES)))
    else {
        return Ok(());
    };
    let Some(open) = argument_start(node)
        .and_then(|start| start.checked_sub(node.start_byte()))
        .filter(|open| *open < text.len())
    else {
        return Ok(());
    };
    let Some((receiver, member)) = text[..open].rsplit_once('.') else {
        return Ok(());
    };
    let receiver = receiver.trim();
    let member = member.trim();
    builder.bridge.charge_work(text.len())?;
    if builder.language() == SourceLanguage::Rust && member == "route" {
        return scan_rust_router(
            builder,
            LiteralRoute {
                text,
                offset: node.start_byte(),
                open,
                method: "ANY",
            },
        );
    }
    if builder.language() == SourceLanguage::Go
        && matches!(member, "Handle" | "HandleFunc")
        && matches!(receiver, "http" | "mux")
    {
        return scan_handle(
            builder,
            LiteralRoute {
                text,
                offset: node.start_byte(),
                open,
                method: "ANY",
            },
        );
    }
    let method = route_method(builder.language(), (receiver, member), vapor);
    if let Some(method) = method.filter(|method| HTTP_METHODS.contains(&method.as_str())) {
        emit_literal(
            builder,
            LiteralRoute {
                text,
                offset: node.start_byte(),
                open,
                method: &method,
            },
        )?;
    }
    Ok(())
}

fn route_method(language: SourceLanguage, member: (&str, &str), vapor: bool) -> Option<String> {
    let (receiver, member) = member;
    match language {
        SourceLanguage::CSharp => member.strip_prefix("Map").map(str::to_ascii_uppercase),
        SourceLanguage::Go if HTTP_METHODS.contains(&member) => Some(member.to_owned()),
        SourceLanguage::Go if receiver == "r" => Some(member.to_ascii_uppercase()),
        SourceLanguage::Swift if receiver == "app" || vapor => Some(member.to_ascii_uppercase()),
        _ => None,
    }
}

fn argument_start(node: Node<'_>) -> Option<usize> {
    if let Some(arguments) = node.child_by_field_name("arguments") {
        return Some(arguments.start_byte());
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| match child.kind() {
            "argument_list" | "arguments" => Some(child.start_byte()),
            "call_suffix" => child
                .named_child(0)
                .filter(|arguments| arguments.kind() == "value_arguments")
                .map(|arguments| arguments.start_byte()),
            _ => None,
        })
}

fn scan_rust_router(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: LiteralRoute<'_>,
) -> Result<(), ExtractError> {
    let Some(path) = quoted_after(input.text, input.open + 1) else {
        return Ok(());
    };
    let comma = skip_ascii_whitespace(input.text, path.quote_end + 1);
    if input.text.as_bytes().get(comma) != Some(&b',') {
        return Ok(());
    }
    let method_start = skip_ascii_whitespace(input.text, comma + 1);
    let Some((_, method)) = super::javascript_identifier_at(input.text, method_start) else {
        return Ok(());
    };
    let method = method.to_ascii_uppercase();
    if !HTTP_METHODS.contains(&method.as_str()) {
        return Ok(());
    }
    emit_literal(
        builder,
        LiteralRoute {
            method: &method,
            ..input
        },
    )
}

fn scan_handle(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: LiteralRoute<'_>,
) -> Result<(), ExtractError> {
    let first = skip_ascii_whitespace(input.text, input.open + 1);
    let Some(pattern) = quoted_after(input.text, first)
        .filter(|pattern| pattern.start == first + 1 && complete_literal(input.text, pattern))
    else {
        return Ok(());
    };
    let (method, path) = match pattern.value.split_once(' ') {
        Some((method, path)) if HTTP_METHODS.contains(&method) => (method, path.trim_start()),
        _ => ("ANY", pattern.value),
    };
    if !path.starts_with('/') {
        return Ok(());
    }
    builder.add_route(FrameworkRouteInput {
        method,
        path,
        start: input.offset + pattern.start,
        end: input.offset + pattern.end,
        command: false,
        handler: None,
    })
}

fn scan_cobra(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, Node<'_>),
) -> Result<(), ExtractError> {
    let (source, node) = input;
    let Some(kind) = node.child_by_field_name("type") else {
        return Ok(());
    };
    if node_text(source, kind) != Some("cobra.Command") {
        return Ok(());
    }
    let Some(body) = node.child_by_field_name("body") else {
        return Ok(());
    };
    let mut cursor = body.walk();
    for entry in body.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if entry
            .child_by_field_name("key")
            .and_then(|key| node_text(source, key))
            != Some("Use")
        {
            continue;
        }
        let Some(value) = entry
            .child_by_field_name("value")
            .and_then(|value| value.named_child(0))
        else {
            continue;
        };
        if !matches!(
            value.kind(),
            "interpreted_string_literal" | "raw_string_literal"
        ) {
            continue;
        }
        let Some(command) = node_text(source, value).and_then(|text| quoted_after(text, 0)) else {
            continue;
        };
        builder.add_route(FrameworkRouteInput {
            method: "CMD",
            path: command.value,
            start: value.start_byte() + command.start,
            end: value.start_byte() + command.end,
            command: true,
            handler: None,
        })?;
    }
    Ok(())
}

fn method_argument<'tree>(source: &str, node: Node<'tree>) -> Option<Node<'tree>> {
    let arguments = node
        .named_child(0)
        .filter(|call| call.kind() == "call")?
        .child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .find(|node| {
            node.kind() == "keyword_argument"
                && node
                    .child_by_field_name("name")
                    .and_then(|name| node_text(source, name))
                    == Some("methods")
        })
        .and_then(|node| node.child_by_field_name("value"))
}

fn emit_flask_methods(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, Node<'_>),
    route: LiteralRoute<'_>,
) -> Result<(), ExtractError> {
    builder.bridge.charge_work(route.text.len())?;
    let Some(methods) = flask_methods(input.0, input.1) else {
        return Ok(());
    };
    for method in methods.into_iter().flatten() {
        let method = method.to_ascii_uppercase();
        emit_literal(
            builder,
            LiteralRoute {
                method: &method,
                ..route
            },
        )?;
    }
    Ok(())
}

fn flask_methods<'s>(
    source: &'s str,
    value: Node<'_>,
) -> Option<[Option<&'s str>; MAX_ROUTE_METHODS]> {
    if value.kind() != "list" {
        return None;
    }
    let mut methods = [None; MAX_ROUTE_METHODS];
    let mut cursor = value.walk();
    for (index, node) in value.named_children(&mut cursor).enumerate() {
        let text = node_text(source, node)?;
        let quoted = quoted_after(text, 0)?;
        if quoted.start != 1
            || quoted.quote_end + 1 != text.len()
            || !HTTP_METHODS
                .iter()
                .any(|method| method.eq_ignore_ascii_case(quoted.value))
        {
            return None;
        }
        *methods.get_mut(index)? = Some(quoted.value);
    }
    Some(methods)
}

pub(super) fn complete_literal(text: &str, quoted: &Quoted<'_>) -> bool {
    matches!(
        text.as_bytes()
            .get(skip_ascii_whitespace(text, quoted.quote_end + 1)),
        Some(b',' | b')')
    )
}
