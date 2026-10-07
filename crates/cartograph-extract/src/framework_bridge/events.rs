//! Literal event names and handler sites; joins happen in the staged generation.

use super::{
    BTreeMap, ExtractError, FrameworkBuilder, FrameworkReferenceInput, ObjcCodeMarker,
    ReferenceKind, SourceLanguage, SymbolId, add_event_landmark, bounded_bridge_range,
    bridge_literal, bridge_marker, handler_shapes, javascript_bindings, javascript_shapes,
    native_emitters, next_objc_code_marker, skip_ascii_whitespace, source_range,
};

const MAX_SYNTAX_ANCESTORS: usize = 16;

struct Subscription<'tree> {
    call: tree_sitter::Node<'tree>,
    event: tree_sitter::Node<'tree>,
    handler: Option<tree_sitter::Node<'tree>>,
}

struct Evidence<'a> {
    unique: &'a BTreeMap<String, usize>,
    imports: &'a javascript_bindings::Imports,
    emitters: native_emitters::Emitters,
}

pub(super) fn next_producer(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (mut cursor, name): (usize, &str),
) -> Result<Option<usize>, ExtractError> {
    if builder.language() == SourceLanguage::ObjectiveC {
        return next_objc_code_marker(
            builder,
            ObjcCodeMarker {
                range: source_range(source, 0),
                cursor,
                name,
            },
        );
    }
    while let Some(start) = bridge_marker(builder, source, (cursor, name))? {
        cursor = start + name.len();
        let boundary = name.starts_with('.')
            || start
                .checked_sub(1)
                .and_then(|index| source.as_bytes().get(index))
                .is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'$'));
        if boundary && code_position(builder, cursor)? {
            return Ok(Some(start));
        }
    }
    Ok(None)
}

fn code_position(builder: &mut FrameworkBuilder<'_, '_>, end: usize) -> Result<bool, ExtractError> {
    let mut node = builder
        .syntax_root()
        .and_then(|root| root.named_descendant_for_byte_range(end.saturating_sub(1), end));
    for _ in 0..MAX_SYNTAX_ANCESTORS {
        builder.bridge.charge_work(1)?;
        let Some(current) = node else {
            return Ok(true);
        };
        if current.kind().contains("string") || current.kind().contains("comment") {
            return Ok(false);
        }
        node = current.parent();
    }
    Ok(false)
}

pub(super) fn consumers(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (unique, imports): (&BTreeMap<String, usize>, &javascript_bindings::Imports),
) -> Result<(), ExtractError> {
    if imports.is_empty() {
        return Ok(());
    }
    let evidence = Evidence {
        unique,
        imports,
        emitters: native_emitters::collect(builder, (imports, unique))?,
    };
    for marker in [".on", ".once", ".addListener"] {
        let mut cursor = 0;
        while let Some(position) = bridge_marker(builder, source, (cursor, marker))? {
            cursor = consume_call(builder, source, (position + marker.len(), &evidence))?;
        }
    }
    Ok(())
}

fn consume_call(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (end, evidence): (usize, &Evidence<'_>),
) -> Result<usize, ExtractError> {
    let Some(Subscription {
        call,
        event,
        handler,
    }) = subscription(builder, (end, evidence))?
    else {
        return Ok(end);
    };
    let Some(literal) = bridge_literal(builder, source_range(source, event.start_byte()))? else {
        return Ok(call.end_byte());
    };
    let Some(owner) = add_event_landmark(builder, ("react-native-event-consumer", &literal))?
    else {
        return Ok(call.end_byte());
    };
    if let Some(handler) = handler {
        add_handler(builder, source, (owner, handler, evidence.unique))?;
    }
    Ok(call.end_byte())
}

fn subscription<'tree>(
    builder: &mut FrameworkBuilder<'tree, '_>,
    (end, evidence): (usize, &Evidence<'_>),
) -> Result<Option<Subscription<'tree>>, ExtractError> {
    let Some(call) = javascript_shapes::call_at(builder, end) else {
        return Ok(None);
    };
    let Some(function) = call
        .child_by_field_name("function")
        .filter(|function| function.end_byte() == end)
    else {
        return Ok(None);
    };
    let Some(receiver) = function.child_by_field_name("object") else {
        return Ok(None);
    };
    builder
        .bridge
        .charge_work(receiver.end_byte() - receiver.start_byte())?;
    if !native_emitters::accepts(
        builder.source(),
        (receiver, call),
        (evidence.imports, &evidence.emitters),
    ) {
        return Ok(None);
    }
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Ok(None);
    };
    if arguments.named_child_count() > javascript_shapes::MAX_ARGUMENT_NODES {
        return Ok(None);
    }
    builder.bridge.charge_work(arguments.named_child_count())?;
    let mut cursor = arguments.walk();
    let mut values = arguments
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra());
    Ok(values
        .next()
        .filter(|node| node.kind() == "string")
        .map(|event| Subscription {
            call,
            event,
            handler: values.next(),
        }))
}

fn add_handler(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (owner, handler, unique): (SymbolId, tree_sitter::Node<'_>, &BTreeMap<String, usize>),
) -> Result<(), ExtractError> {
    let name = if handler.kind() == "member_expression" {
        handler
            .child_by_field_name("property")
            .filter(|node| node.kind() == "property_identifier")
    } else {
        Some(handler).filter(|node| node.kind() == "identifier")
    };
    let Some(name) = name else {
        return Ok(());
    };
    let Some(value) = source.get(name.byte_range()) else {
        return Ok(());
    };
    builder
        .bridge
        .charge_work(name.end_byte() - name.start_byte())?;
    if handler.kind() == "identifier" && unique.get(value) != Some(&1) {
        return Ok(());
    }
    let qualified = if handler.kind() == "member_expression" {
        handler_shapes::lookup(builder, (handler, unique))?
    } else {
        None
    };
    if handler.kind() == "member_expression" && qualified.is_none() {
        return Ok(());
    }
    builder.add_reference(FrameworkReferenceInput {
        owner: Some(owner),
        name: value,
        resolution_name: qualified.as_deref(),
        kind: ReferenceKind::Calls,
        start: name.start_byte(),
        end: name.end_byte(),
    })
}

pub(super) fn literal_start(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    query: (usize, &str),
) -> Result<Option<usize>, ExtractError> {
    let (end, marker) = query;
    let range = bounded_bridge_range(source_range(source, end));
    let separator = if marker == "sendEventWithName:" {
        b':'
    } else {
        b'('
    };
    let open = skip_ascii_whitespace(&source[..range.end], end);
    builder.bridge.charge_work(open - end)?;
    if source.as_bytes().get(open) != Some(&separator) {
        return Ok(None);
    }
    let mut start = open + 1;
    if marker == "sendEvent(withName:" {
        start = skip_ascii_whitespace(&source[..range.end], start);
        let Some(tail) = source
            .get(start..range.end)
            .and_then(|tail| tail.strip_prefix("withName"))
        else {
            return Ok(None);
        };
        let colon = skip_ascii_whitespace(&source[..range.end], range.end - tail.len());
        if source.as_bytes().get(colon) != Some(&b':') {
            return Ok(None);
        }
        start = colon + 1;
    }
    Ok(Some(start))
}
