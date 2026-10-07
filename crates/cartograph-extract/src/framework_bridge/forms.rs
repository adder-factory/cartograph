//! Small native spelling variations, still scoped to structural declarations.

use super::{
    BTreeMap, ExtractError, FrameworkBuilder, MAX_BRIDGE_SCAN_BYTES, NamedSymbolRange,
    ObjcCodeMarker, Quoted, SymbolKind, SymbolRange, add_member_landmark, bounded_bridge_range,
    bridge_argument_body, next_objc_code_marker, quoted_event_after, skip_ascii_whitespace,
};

pub(super) const OBJC_METHOD_MACROS: [&str; 7] = [
    "RCT_EXPORT_METHOD(",
    "RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(",
    "RCT_EXTERN_METHOD(",
    "RCT_EXTERN__BLOCKING_SYNCHRONOUS_METHOD(",
    "RCT_REMAP_METHOD(",
    "RCT_EXTERN_REMAP_METHOD(",
    "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(",
];
pub(super) const VIEW_MACROS: [&str; 3] = [
    "RCT_EXPORT_VIEW_PROPERTY(",
    "RCT_REMAP_VIEW_PROPERTY(",
    "RCT_CUSTOM_VIEW_PROPERTY(",
];
pub(super) const MODULE_MACROS: [&str; 3] = [
    "RCT_EXTERN_REMAP_MODULE(",
    "RCT_EXTERN_MODULE(",
    "RCT_EXPORT_MODULE(",
];
const DEFERRED_EXPORT_BYTES: u64 = 128;

pub(super) struct Registration {
    pub(super) value: Option<(String, usize, usize)>,
    explicit: bool,
}

pub(super) fn register<'source>(
    registrations: &mut BTreeMap<&'source str, Registration>,
    (class, value, explicit): (&'source str, Option<(String, usize, usize)>, bool),
) {
    let Some(previous) = registrations.get_mut(class) else {
        registrations.insert(class, Registration { value, explicit });
        return;
    };
    if explicit && !previous.explicit {
        *previous = Registration { value, explicit };
    } else if previous.explicit == explicit
        && previous.value.as_ref().map(|value| &value.0) != value.as_ref().map(|value| &value.0)
    {
        previous.value = None;
    }
}

pub(super) fn kotlin_constant_pair<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let Some(body) = bridge_argument_body(builder, range)? else {
        return Ok(None);
    };
    let Some(argument) = quoted_event_after(&body.source[..body.end], body.start) else {
        return Ok(None);
    };
    let after = skip_ascii_whitespace(&body.source[..body.end], argument.quote_end + 1);
    builder
        .bridge
        .charge_work(after.saturating_sub(body.start))?;
    let Some(tail) = body.source[after..body.end].strip_prefix("to") else {
        return Ok(None);
    };
    Ok(tail
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_whitespace)
        .then_some(argument)
        .filter(|_| !tail.trim().is_empty()))
}

pub(super) struct DeferredExport {
    module: String,
    name: String,
    start: usize,
    end: usize,
}

pub(super) fn export_member(
    builder: &mut FrameworkBuilder<'_, '_>,
    deferred: &mut Vec<DeferredExport>,
    (range, arguments, module, name, start, end): (
        SymbolRange<'_>,
        usize,
        &str,
        &str,
        usize,
        usize,
    ),
) -> Result<(), ExtractError> {
    let spaced = range
        .source
        .as_bytes()
        .get(arguments.saturating_sub(2))
        .is_some_and(u8::is_ascii_whitespace);
    if !spaced {
        return add_member_landmark(
            builder,
            (SymbolKind::Method, "react-native-method", module),
            (name, start, end),
        );
    }
    builder.bridge.reserve_working_bytes(
        DEFERRED_EXPORT_BYTES
            + u64::try_from(module.len() + name.len()).map_err(|_| ExtractError::OutputLimit)?,
    )?;
    deferred.push(DeferredExport {
        module: module.to_owned(),
        name: name.to_owned(),
        start,
        end,
    });
    Ok(())
}

// Append newly supported spellings after established facts. Their IDs and
// canonical ordering still come from the ordinary landmark and reducer owners.
pub(super) fn append_exports(
    builder: &mut FrameworkBuilder<'_, '_>,
    deferred: Vec<DeferredExport>,
) -> Result<(), ExtractError> {
    for export in deferred {
        builder.bridge.charge_work(1)?;
        add_member_landmark(
            builder,
            (SymbolKind::Method, "react-native-method", &export.module),
            (&export.name, export.start, export.end),
        )?;
    }
    Ok(())
}

pub(super) fn next_macro_arguments(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ObjcCodeMarker<'_, '_>,
) -> Result<Option<usize>, ExtractError> {
    let name = input.name.trim_end_matches('(');
    let mut cursor = input.cursor;
    while let Some(position) = next_objc_code_marker(
        builder,
        ObjcCodeMarker {
            cursor,
            name,
            ..input
        },
    )? {
        cursor = position + name.len();
        let bounded = bounded_bridge_range(SymbolRange {
            start: cursor,
            ..input.range
        });
        let open = skip_ascii_whitespace(&input.range.source[..bounded.end], cursor);
        builder.bridge.charge_work(open - cursor)?;
        if input.range.source.as_bytes().get(open) == Some(&b'(') {
            return Ok(Some(open + 1));
        }
    }
    Ok(None)
}

pub(super) fn has_macro(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'_>,
    names: &[&str],
) -> Result<bool, ExtractError> {
    for name in names {
        if next_macro_arguments(
            builder,
            ObjcCodeMarker {
                range,
                cursor: range.start,
                name,
            },
        )?
        .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn swift_attribute<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: NamedSymbolRange<'source, '_>,
) -> Result<Option<&'source str>, ExtractError> {
    let range = input.range;
    let Some(node) = builder
        .syntax_root()
        .and_then(|root| root.named_descendant_for_byte_range(range.start, range.end))
    else {
        return Ok(None);
    };
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if let Some(text) = named_attribute(builder, (range.source, child, input.name))? {
            return Ok(Some(text));
        }
        if child.kind() != "modifiers" {
            continue;
        }
        let mut walk = child.walk();
        for attribute in child.named_children(&mut walk) {
            builder.bridge.charge_work(1)?;
            if let Some(text) = named_attribute(builder, (range.source, attribute, input.name))? {
                return Ok(Some(text));
            }
        }
    }
    Ok(None)
}

fn named_attribute<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    (source, node, name): (&'source str, tree_sitter::Node<'_>, &str),
) -> Result<Option<&'source str>, ExtractError> {
    if node.kind() != "attribute" || node.end_byte() - node.start_byte() > MAX_BRIDGE_SCAN_BYTES {
        return Ok(None);
    }
    builder
        .bridge
        .charge_work(node.end_byte() - node.start_byte())?;
    let Some(text) = source.get(node.byte_range()) else {
        return Ok(None);
    };
    let marker = format!("@{name}");
    Ok(text
        .strip_prefix(&marker)
        .filter(|tail| {
            tail.as_bytes()
                .first()
                .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
        })
        .map(|_| text))
}
