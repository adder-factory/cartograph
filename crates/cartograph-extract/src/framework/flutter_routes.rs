//! Flutter routes read from direct argument and map-entry nodes.

use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use tree_sitter::Node;

use super::{DelimiterInput, matching_delimiter, segments::Segments};
use super::{
    FrameworkBuilder, FrameworkReferenceInput, LandmarkInput, Quoted, javascript_identifier_at,
    quoted_after, safe_route_value, skip_ascii_whitespace, syntax_nodes::SyntaxNodes,
};
use crate::ExtractError;
use crate::code_scan::CodeScan;

const MAX_BUILDER_BYTES: usize = 4_096;
const CALL_PARENT_STEPS: usize = 3;
const SOURCE_POLL_BYTES: usize = 256;
const MAX_TYPE_ARGUMENT_BYTES: usize = 256;
const MAX_TYPE_ARGUMENT_DEPTH: usize = 16;
const LOCAL_NAME_ENTRY_BYTES: u64 = 192;

struct ImportPolicy {
    opaque: bool,
    local: BTreeSet<String>,
}

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Dart
        || (!source.contains("GoRoute") && !source.contains("MaterialApp"))
    {
        return Ok(());
    }
    let Some(root) = builder.syntax_root() else {
        return Ok(());
    };
    let imports = import_policy(builder, root)?;
    for node in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        if node.kind() != "arguments" {
            continue;
        }
        let Some(callee) = callee(node) else {
            continue;
        };
        match source.get(callee.byte_range()) {
            Some("GoRoute") => scan_go_route(builder, (source, node, callee), &imports)?,
            Some("MaterialApp") => scan_material_app(builder, (source, node), &imports)?,
            _ => {}
        }
    }
    scan_material_text(builder, source, &imports)?;
    Ok(())
}

fn scan_material_text(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    imports: &ImportPolicy,
) -> Result<(), ExtractError> {
    let mut scan = CodeScan::new(source.as_bytes());
    for chunk in (0..source.len()).step_by(SOURCE_POLL_BYTES) {
        let boundary = source.len().min(chunk + SOURCE_POLL_BYTES);
        builder.bridge.charge_work(boundary - chunk)?;
        while let Some((start, byte)) = scan.next_bounded(boundary) {
            if byte == b'M' {
                scan_material_site(builder, (source, start), imports)?;
            }
        }
    }
    Ok(())
}

fn scan_material_site(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, usize),
    imports: &ImportPolicy,
) -> Result<(), ExtractError> {
    let (source, start) = input;
    if !source[start..].starts_with("MaterialApp")
        || !super::marker_has_identifier_boundary(source, start, "MaterialApp")
    {
        return Ok(());
    }
    let open = skip_ascii_whitespace(source, start + "MaterialApp".len());
    let Some(close) = matching_delimiter(DelimiterInput::bounded_parentheses(
        source,
        open,
        MAX_BUILDER_BYTES,
    )) else {
        return Ok(());
    };
    builder.bridge.charge_work(close - open)?;
    for (offset, argument) in Segments::new(&source[open + 1..close], b',') {
        builder.check_cancelled()?;
        let leading = argument.len() - argument.trim_start().len();
        let Some(body) = argument.trim_start().strip_prefix("routes:") else {
            continue;
        };
        let map_start = skip_ascii_whitespace(body, 0);
        let Some(map_end) = matching_delimiter(DelimiterInput::braces(body, map_start)) else {
            continue;
        };
        let base = open + 1 + offset + leading + "routes:".len() + map_start + 1;
        scan_material_entries(builder, (&body[map_start + 1..map_end], base), imports)?;
    }
    Ok(())
}

fn scan_material_entries(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, usize),
    imports: &ImportPolicy,
) -> Result<(), ExtractError> {
    let (body, base) = input;
    for (offset, entry) in Segments::new(body, b',') {
        builder.check_cancelled()?;
        let first = skip_ascii_whitespace(entry, 0);
        let Some(path) = quoted_after(entry, first).filter(|path| path.start == first + 1) else {
            continue;
        };
        if entry
            .as_bytes()
            .get(skip_ascii_whitespace(entry, path.quote_end + 1))
            != Some(&b':')
        {
            continue;
        }
        let path = path.with_offset(base + offset);
        let anchor = path.start;
        let anchor_end = path.end;
        let handler = widget_text(entry, base + offset);
        emit(
            builder,
            FlutterRoute {
                path,
                anchor,
                anchor_end,
                handler,
            },
            imports,
        )?;
    }
    Ok(())
}

fn callee(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..CALL_PARENT_STEPS {
        node = node.parent()?;
        if node.kind() == "selector" {
            return node
                .prev_named_sibling()
                .filter(|head| matches!(head.kind(), "identifier" | "type_identifier"));
        }
        if matches!(node.kind(), "new_expression" | "const_object_expression") {
            let mut cursor = node.walk();
            return node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "type_identifier");
        }
    }
    None
}

fn named_argument<'tree>(input: (&str, Node<'tree>), name: &str) -> Option<Node<'tree>> {
    let (source, arguments) = input;
    let mut cursor = arguments.walk();
    arguments.named_children(&mut cursor).find(|child| {
        child.kind() == "named_argument"
            && child
                .named_child(0)
                .and_then(|label| source.get(label.byte_range()))
                .is_some_and(|label| label.trim_end_matches(':') == name)
    })
}

fn scan_go_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, Node<'_>, Node<'_>),
    imports: &ImportPolicy,
) -> Result<(), ExtractError> {
    let (source, arguments, callee) = input;
    let Some(path_node) =
        named_argument((source, arguments), "path").and_then(|argument| argument.named_child(1))
    else {
        return Ok(());
    };
    let Some(path) = literal(source, path_node) else {
        return Ok(());
    };
    let handler = match named_argument((source, arguments), "builder") {
        Some(node) => widget(builder, source, node)?,
        None => None,
    };
    emit(
        builder,
        FlutterRoute {
            path,
            anchor: callee.start_byte(),
            anchor_end: callee.end_byte(),
            handler,
        },
        imports,
    )
}

fn scan_material_app(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (&str, Node<'_>),
    imports: &ImportPolicy,
) -> Result<(), ExtractError> {
    let (source, arguments) = input;
    let Some(map) = named_argument((source, arguments), "routes")
        .and_then(|argument| argument.named_child(1))
        .filter(|map| map.kind() == "set_or_map_literal")
    else {
        return Ok(());
    };
    let mut cursor = map.walk();
    for entry in map.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        let Some(key) = entry
            .child_by_field_name("key")
            .filter(|_| entry.kind() == "pair")
        else {
            continue;
        };
        let Some(path) = literal(source, key) else {
            continue;
        };
        let handler = widget(builder, source, entry)?;
        let anchor = path.start;
        let anchor_end = path.end;
        emit(
            builder,
            FlutterRoute {
                path,
                anchor,
                anchor_end,
                handler,
            },
            imports,
        )?;
    }
    Ok(())
}

fn literal<'s>(source: &'s str, node: Node<'_>) -> Option<Quoted<'s>> {
    if node.kind() != "string_literal" {
        return None;
    }
    let text = source
        .get(node.byte_range())
        .filter(|text| text.len() <= MAX_BUILDER_BYTES)?;
    let quoted = quoted_after(text, 0)?;
    if quoted.start != 1 || quoted.quote_end + 1 != text.len() || quoted.value.contains(['$', '\\'])
    {
        return None;
    }
    Some(quoted.with_offset(node.start_byte()))
}

fn widget<'s>(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &'s str,
    node: Node<'_>,
) -> Result<Option<(&'s str, usize, usize)>, ExtractError> {
    let Some(text) = source
        .get(node.byte_range())
        .filter(|text| text.len() <= MAX_BUILDER_BYTES)
    else {
        return Ok(None);
    };
    builder.bridge.charge_work(text.len())?;
    Ok(widget_text(text, node.start_byte()))
}

fn widget_text(text: &str, offset: usize) -> Option<(&str, usize, usize)> {
    let mut start = skip_ascii_whitespace(text, direct_arrow(text)? + "=>".len());
    let (mut end, mut name) = javascript_identifier_at(text, start)?;
    if matches!(name, "const" | "new") {
        start = skip_ascii_whitespace(text, end);
        (end, name) = javascript_identifier_at(text, start)?;
    }
    if !name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        || text.as_bytes().get(constructor_open(text, end)?) != Some(&b'(')
    {
        return None;
    }
    Some((name, offset + start, offset + end))
}

fn constructor_open(text: &str, end: usize) -> Option<usize> {
    let open = skip_ascii_whitespace(text, end);
    if text.as_bytes().get(open) != Some(&b'<') {
        return Some(open);
    }
    let mut depth = 0_usize;
    for (offset, byte) in text.as_bytes()[open..]
        .iter()
        .copied()
        .take(MAX_TYPE_ARGUMENT_BYTES)
        .enumerate()
    {
        match byte {
            b'<' => {
                depth += 1;
                if depth > MAX_TYPE_ARGUMENT_DEPTH {
                    return None;
                }
            }
            b'>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(skip_ascii_whitespace(text, open + offset + 1));
                }
            }
            b',' | b'.' | b'?' | b'_' => {}
            byte if byte.is_ascii_alphanumeric() || byte.is_ascii_whitespace() => {}
            _ => return None,
        }
    }
    None
}

fn direct_arrow(text: &str) -> Option<usize> {
    let mut depth = 0_usize;
    for (index, byte) in CodeScan::new(text.as_bytes()) {
        match byte {
            b'(' | b'[' => depth = depth.saturating_add(1),
            b')' | b']' => depth = depth.saturating_sub(1),
            b'{' if depth == 0 => return None,
            b'=' if depth == 0 && text.as_bytes().get(index + 1) == Some(&b'>') => {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy)]
struct FlutterRoute<'s> {
    path: Quoted<'s>,
    anchor: usize,
    anchor_end: usize,
    handler: Option<(&'s str, usize, usize)>,
}

fn emit(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: FlutterRoute<'_>,
    imports: &ImportPolicy,
) -> Result<(), ExtractError> {
    let Some(raw) = safe_route_value(input.path.value, false) else {
        return Ok(());
    };
    let path = if raw.starts_with('/') {
        raw
    } else {
        format!("/{raw}")
    };
    let route = builder.add_landmark_with_id(LandmarkInput {
        kind: SymbolKind::Route,
        name: format!("ANY {path}"),
        identity: format!("any::{path}"),
        start: input.anchor,
        end: input.anchor_end,
        body_search_text: format!("route ANY {path}"),
        target: None,
    })?;
    if let Some(owner) = route
        && let Some((name, start, end)) = input.handler
    {
        // Combinators/conditional imports are not represented by the native
        // wildcard binding. Keep the reference without guessing its library.
        let lookup = (imports.opaque && !imports.local.contains(name))
            .then(|| format!("cartograph.flutter-unproven::{name}"));
        for kind in [ReferenceKind::Calls, ReferenceKind::References] {
            builder.add_reference(FrameworkReferenceInput {
                owner: Some(owner.clone()),
                name,
                resolution_name: lookup.as_deref(),
                kind,
                start,
                end,
            })?;
        }
    }
    Ok(())
}

fn import_policy(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<ImportPolicy, ExtractError> {
    let mut policy = ImportPolicy {
        opaque: imports_opaque(builder, root)?,
        local: BTreeSet::new(),
    };
    for index in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        let Some(symbol) = builder.original_symbol(index).filter(|symbol| {
            matches!(
                symbol.kind,
                SymbolKind::Class | SymbolKind::Function | SymbolKind::TypeAlias
            )
        }) else {
            continue;
        };
        let name = symbol.name.clone();
        builder
            .bridge
            .reserve_working_bytes(LOCAL_NAME_ENTRY_BYTES.saturating_add(
                u64::try_from(name.len()).map_err(|_| ExtractError::OutputLimit)?,
            ))?;
        policy.local.insert(name);
    }
    Ok(policy)
}

fn imports_opaque(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<bool, ExtractError> {
    for node in SyntaxNodes::new(root) {
        builder.bridge.charge_work(1)?;
        if node.kind() == "combinator"
            || (node.kind() == "configurable_uri" && node.named_child_count() > 1)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::{NativeExtractor, SourceLimits, SourceSnapshot, framework::FrameworkInput};

    const LONG_LITERAL_BYTES: usize = 900_000;
    const CANCEL_AFTER_POLL: usize = 8;

    #[test]
    fn material_source_scan_polls_cancellation_inside_long_literals()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = format!(
            "void f(){{MaterialApp(routes:{{}});final s=\"{}\";}}",
            "a".repeat(LONG_LITERAL_BYTES)
        );
        let snapshot = SourceSnapshot::from_bytes_for_capability_validation(
            "lib/router.dart",
            source.as_bytes(),
            SourceLimits::new(source.len())?,
        )?;
        let file = NativeExtractor::new_for_capability_validation(SourceLanguage::Dart)?
            .extract(&snapshot)?;
        let armed = Cell::new(false);
        let polls = Cell::new(0_usize);
        let mut cancelled = || {
            if !armed.get() {
                return false;
            }
            polls.set(polls.get() + 1);
            polls.get() == CANCEL_AFTER_POLL
        };
        let mut builder =
            FrameworkBuilder::new(FrameworkInput::new(&snapshot, file), &mut cancelled)?;
        let imports = ImportPolicy {
            opaque: false,
            local: BTreeSet::new(),
        };
        armed.set(true);
        let result = scan_material_text(&mut builder, &source, &imports);
        assert_eq!(result, Err(ExtractError::Cancelled));
        assert_eq!(polls.get(), CANCEL_AFTER_POLL);
        Ok(())
    }
}
