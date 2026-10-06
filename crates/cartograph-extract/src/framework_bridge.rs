mod ownership;

use std::collections::{BTreeMap, BTreeSet};

use ownership::{AnnotationSite, BridgeCall, BridgeOwnership};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};

use crate::{
    ExtractError,
    framework::{
        FrameworkBuilder, FrameworkNearReferenceInput, FrameworkReferenceInput, LandmarkInput,
        Quoted, consume_quote_state, javascript_identifier_at as identifier_at, quoted_after,
        skip_ascii_whitespace,
    },
};

const MAX_BRIDGE_SCAN_BYTES: usize = 4_096;
const REGISTRY_FACTORIES: [&str; 4] = [
    "TurboModuleRegistry.get",
    "TurboModuleRegistry.getEnforcing",
    "requireNativeModule",
    "requireOptionalNativeModule",
];
const MAX_NATIVE_ALIASES: usize = 256;
const NATIVE_ALIAS_ENTRY_BYTES: usize = 64;
const MAX_SWIFT_SELECTOR_BYTES: usize = 512;
const MAX_SWIFT_ATTRIBUTE_LINES: usize = 8;
const CANCELLATION_INTERVAL_BRIDGE_BYTES: usize = 256;
const OBJC_SCOPE_ENTRY_BYTES: usize = 256;
const EXPO_EXPORT_ENTRY_BYTES: u64 = 64;
/// Every React Native method-export macro name starts with this prefix.
const REACT_NATIVE_MACRO_PREFIX: &str = "RCT_";
/// Objective-C containers terminate independently, including extern interfaces.
const OBJC_CONTAINER_MARKERS: [&str; 4] = ["@implementation", "@interface", "@protocol", "@end"];

#[derive(Clone, Copy)]
struct SymbolRange<'source> {
    source: &'source str,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct NamedSymbolRange<'source, 'name> {
    range: SymbolRange<'source>,
    name: &'name str,
}

struct ExpoDefinitionScan<'seen, 'source> {
    definition: usize,
    include_outside: bool,
    seen: &'seen mut BTreeSet<(usize, &'source str, &'source str)>,
}

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let checkpoint = builder.bridge.checkpoint();
    match scan_bounded(builder, source) {
        Ok(()) if builder.bridge.retained_output_fits() => Ok(()),
        Ok(()) | Err(ExtractError::OutputLimit) => {
            builder.bridge.omit_facts(checkpoint);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn scan_bounded(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    builder.check_cancelled()?;
    if matches!(
        builder.language(),
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::ObjectiveC
            | SourceLanguage::Java
            | SourceLanguage::Kotlin
            | SourceLanguage::Swift
    ) {
        builder.bridge.index_owners()?;
    }
    match builder.language() {
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => scan_javascript(builder, source),
        SourceLanguage::ObjectiveC => scan_objc(builder, source),
        SourceLanguage::Java | SourceLanguage::Kotlin | SourceLanguage::Swift => {
            let ownership = BridgeOwnership::build(builder, source)?;
            if builder.language() == SourceLanguage::Swift {
                scan_swift(builder, source, &ownership)
            } else {
                scan_jvm(builder, source, &ownership)
            }
        }
        _ => Ok(()),
    }
}

fn scan_javascript(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    scan_native_modules_calls(builder, source)?;
    scan_registry_modules(builder, source)?;
    scan_registry_alias_calls(builder, source)?;
    scan_turbo_module_spec(builder, source)?;
    scan_codegen_components(builder, source)?;
    scan_javascript_event_consumers(builder, source)
}

fn scan_javascript_event_consumers(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let mut native_context = false;
    for marker in [
        "NativeEventEmitter",
        "DeviceEventEmitter",
        "NativeModules",
        "requireNativeModule",
        "react-native",
    ] {
        if bridge_marker(builder, source, (0, marker))?.is_some() {
            native_context = true;
            break;
        }
    }
    if !native_context {
        return Ok(());
    }
    let mut cursor = 0_usize;
    while let Some(position) = bridge_marker(builder, source, (cursor, ".addListener("))? {
        let call = position + ".addListener(".len();
        let Some(event) = bridge_literal(builder, source_range(source, call))? else {
            cursor = call;
            continue;
        };
        let event_id = add_event_landmark(builder, ("react-native-event-consumer", &event))?;
        let bounded = bounded_bridge_range(source_range(source, event.end));
        builder.bridge.charge_work(bounded.end - bounded.start)?;
        if let Some(event_id) = event_id
            && let Some(comma) = source[event.end..bounded.end]
                .find(',')
                .map(|offset| event.end + offset + 1)
        {
            let handler_start = skip_ascii_whitespace(&source[..bounded.end], comma);
            if let Some((handler_end, handler)) = bridge_identifier(SymbolRange {
                start: handler_start,
                ..bounded
            }) {
                builder.add_reference(FrameworkReferenceInput {
                    owner: Some(event_id),
                    name: handler,
                    resolution_name: None,
                    kind: ReferenceKind::Calls,
                    start: handler_start,
                    end: handler_end,
                })?;
            }
        }
        cursor = event.end;
    }
    Ok(())
}

fn scan_native_event_producers(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    markers: &[&str],
) -> Result<(), ExtractError> {
    let mut seen = BTreeSet::new();
    for marker in markers {
        let mut cursor = 0_usize;
        while let Some(position) = bridge_marker(builder, source, (cursor, marker))? {
            let call = position + marker.len();
            let Some(event) = bridge_literal(builder, source_range(source, call))? else {
                cursor = call;
                continue;
            };
            if seen.insert((event.start, event.end)) {
                add_event_landmark(builder, ("react-native-event-producer", &event))?;
            }
            cursor = event.end;
        }
    }
    Ok(())
}

fn add_event_landmark(
    builder: &mut FrameworkBuilder<'_, '_>,
    (category, event): (&str, &Quoted<'_>),
) -> Result<Option<SymbolId>, ExtractError> {
    if !safe_event_name(event.value) {
        return Ok(None);
    }
    builder.add_landmark_with_id(LandmarkInput {
        kind: SymbolKind::Resource,
        name: event.value.to_owned(),
        identity: format!("{category}::{}", event.value),
        start: event.start,
        end: event.end,
        body_search_text: format!("react native event channel {} {category}", event.value),
        target: None,
    })
}

fn safe_event_name(event: &str) -> bool {
    !event.is_empty()
        && event.len() <= 512
        && event.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
        && ![
            "password",
            "passwd",
            "secret",
            "token",
            "apikey",
            "privatekey",
            "credential",
        ]
        .into_iter()
        .any(|word| event.to_ascii_lowercase().contains(word))
}

fn quoted_event_after(value: &str, from: usize) -> Option<Quoted<'_>> {
    let mut cursor = skip_ascii_whitespace(value, from);
    if value.as_bytes().get(cursor) == Some(&b'@') {
        cursor = cursor.saturating_add(1);
    }
    let quote = *value.as_bytes().get(cursor)?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    quoted_after(value, cursor)
}

fn scan_registry_alias_calls(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let bindings = collect_registry_alias_bindings(builder, source)?;
    for (alias, module) in bindings {
        scan_registry_alias_invocations(builder, source, (&alias, &module))?;
    }
    Ok(())
}

fn collect_registry_alias_bindings(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<Vec<(String, String)>, ExtractError> {
    let mut bindings = Vec::new();
    let mut seen_calls = BTreeSet::new();
    for marker in REGISTRY_FACTORIES {
        let mut cursor = 0;
        while bindings.len() < MAX_NATIVE_ALIASES
            && let Some(call_start) = bridge_marker(builder, source, (cursor, marker))?
        {
            cursor = call_start + marker.len();
            let Some(module) = legacy_bridge_argument(builder, source, cursor)? else {
                continue;
            };
            let prefix = bounded_bridge_prefix(source, call_start);
            builder.bridge.charge_work(prefix.len().saturating_mul(4))?;
            if seen_calls.insert(call_start)
                && let Some(alias) = assigned_identifier_before(prefix, prefix.len())
            {
                builder.bridge.reserve_working_bytes(
                    u64::try_from(alias.len() + module.value.len() + NATIVE_ALIAS_ENTRY_BYTES)
                        .map_err(|_| ExtractError::OutputLimit)?,
                )?;
                bindings.push((alias.to_owned(), module.value.to_owned()));
            }
            cursor = module.end;
        }
    }
    Ok(bindings)
}

fn scan_registry_alias_invocations(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (alias, module): (&str, &str),
) -> Result<(), ExtractError> {
    let marker = format!("{alias}.");
    let mut cursor = 0_usize;
    while let Some(alias_start) = bridge_marker(builder, source, (cursor, &marker))? {
        let bounded = bounded_bridge_range(source_range(source, alias_start));
        builder.bridge.charge_work(bounded.end - bounded.start)?;
        if alias_start > 0 && source.as_bytes()[alias_start - 1].is_ascii_alphanumeric() {
            cursor = alias_start + marker.len();
            continue;
        }
        let method_start = alias_start + marker.len();
        let Some((method_end, method)) = bridge_identifier(SymbolRange {
            start: method_start,
            ..bounded
        }) else {
            cursor = method_start;
            continue;
        };
        let call = skip_ascii_whitespace(&source[..bounded.end], method_end);
        if source.as_bytes()[..bounded.end].get(call) == Some(&b'(')
            && !react_native_blocklisted(method)
        {
            builder.add_reference_near_with_resolution(FrameworkNearReferenceInput {
                name: method,
                resolution_name: Some(&format!("{module}::{method}")),
                kind: ReferenceKind::Calls,
                start: method_start,
                end: method_end,
            })?;
        }
        cursor = method_end;
    }
    Ok(())
}

fn assigned_identifier_before(source: &str, call_start: usize) -> Option<&str> {
    let boundary = source[..call_start]
        .rfind(['\n', ';'])
        .map_or(0, |offset| offset + 1);
    let statement = source[boundary..call_start].trim();
    let (declaration, rhs) = statement.rsplit_once('=')?;
    if !rhs.trim().is_empty() {
        return None;
    }
    let declaration = declaration.trim();
    let declaration = ["const", "let", "var"]
        .into_iter()
        .find_map(|keyword| declaration.strip_prefix(keyword))?
        .trim_start();
    let (_, alias) = identifier_at(declaration, 0)?;
    Some(alias)
}

fn legacy_bridge_argument<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &'source str,
    start: usize,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let range = bounded_bridge_range(source_range(source, start));
    let Some(open) = bridge_marker(builder, &source[..range.end], (start, "("))? else {
        return Ok(None);
    };
    builder.bridge.charge_work(range.end - (open + 1))?;
    Ok(quoted_after(&source[..range.end], open + 1))
}

fn bridge_body_range(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    start: usize,
) -> Result<Option<(usize, usize)>, ExtractError> {
    let range = bounded_bridge_range(source_range(source, start));
    let Some(open) = bridge_marker(builder, &source[..range.end], (start, "{"))? else {
        return Ok(None);
    };
    let close = bridge_delimiter_close(
        SymbolRange {
            start: open,
            ..range
        },
        (b'{', b'}'),
        &mut |units| builder.bridge.charge_work(units),
    )?;
    Ok(close.map(|close| (open, close)))
}

fn bounded_bridge_prefix(source: &str, end: usize) -> &str {
    let mut start = end.saturating_sub(MAX_BRIDGE_SCAN_BYTES);
    while !source.is_char_boundary(start) {
        start += 1;
    }
    &source[start..end]
}

fn bridge_literal<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let range = bounded_bridge_range(range);
    builder.bridge.charge_work(range.end - range.start)?;
    Ok(quoted_event_after(&range.source[..range.end], range.start))
}

fn scan_turbo_module_spec(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let base = builder.path().rsplit('/').next().unwrap_or(builder.path());
    if !((base.starts_with("Native") && matches!(file_suffix(base), Some("ts" | "tsx")))
        || (base.contains("Spec.") && matches!(file_suffix(base), Some("ts" | "tsx"))))
    {
        return Ok(());
    }
    let Some(module) = registry_module_name(builder, source)? else {
        return Ok(());
    };
    let Some((open, close)) = interface_body(builder, source, "Spec")? else {
        return Ok(());
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        builder.bridge.charge_work(close - cursor)?;
        while cursor < close && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b';') {
            cursor += 1;
        }
        let statement_start = cursor;
        let method = identifier_at(source, cursor);
        if let Some((name_end, name)) = method {
            let after_name = skip_ascii_whitespace(source, name_end);
            if bytes.get(after_name) == Some(&b'(') && !react_native_blocklisted(name) {
                add_member_landmark(
                    builder,
                    (SymbolKind::Method, "turbo-module-spec-method", module.value),
                    (name, cursor, name_end),
                )?;
            }
        }
        cursor = next_interface_statement(builder, source, (statement_start, close))?;
    }
    Ok(())
}

fn file_suffix(path: &str) -> Option<&str> {
    path.rsplit_once('.').map(|(_, suffix)| suffix)
}

fn registry_module_name<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &'source str,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let mut selected = None;
    for marker in &REGISTRY_FACTORIES[..2] {
        let Some(start) = bridge_marker(builder, source, (0, marker))? else {
            continue;
        };
        let Some(module) = legacy_bridge_argument(builder, source, start + marker.len())? else {
            continue;
        };
        if selected
            .as_ref()
            .is_none_or(|retained: &Quoted<'_>| module.start < retained.start)
        {
            selected = Some(module);
        }
    }
    Ok(selected)
}

fn interface_body(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    expected_name: &str,
) -> Result<Option<(usize, usize)>, ExtractError> {
    let mut cursor = 0;
    while let Some(position) = bridge_marker(builder, source, (cursor, "interface"))? {
        cursor = position + "interface".len();
        let range = bounded_bridge_range(source_range(source, cursor));
        builder.bridge.charge_work(range.end - range.start)?;
        let name_start = skip_ascii_whitespace(&source[..range.end], cursor);
        let Some((name_end, name)) = bridge_identifier(SymbolRange {
            start: name_start,
            ..range
        }) else {
            continue;
        };
        cursor = name_end;
        if name != expected_name {
            continue;
        }
        return bridge_body_range(builder, source, name_end);
    }
    Ok(None)
}

#[derive(Default)]
struct BridgeDelimiterState {
    paren: usize,
    brace: usize,
    bracket: usize,
    angle: usize,
    quote: Option<u8>,
    escaped: bool,
}

impl BridgeDelimiterState {
    fn update_depth(&mut self, byte: u8, track_angle: bool) -> bool {
        match byte {
            b'(' => self.paren = self.paren.saturating_add(1),
            b')' => self.paren = self.paren.saturating_sub(1),
            b'{' => self.brace = self.brace.saturating_add(1),
            b'}' => self.brace = self.brace.saturating_sub(1),
            b'[' => self.bracket = self.bracket.saturating_add(1),
            b']' => self.bracket = self.bracket.saturating_sub(1),
            b'<' if track_angle => self.angle = self.angle.saturating_add(1),
            b'>' if track_angle => self.angle = self.angle.saturating_sub(1),
            _ => return false,
        }
        true
    }

    const fn top_level(&self) -> bool {
        self.paren == 0 && self.brace == 0 && self.bracket == 0 && self.angle == 0
    }
}

fn next_interface_statement(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    (start, close): (usize, usize),
) -> Result<usize, ExtractError> {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut state = BridgeDelimiterState::default();
    while cursor < close {
        if (cursor - start).is_multiple_of(CANCELLATION_INTERVAL_BRIDGE_BYTES) {
            builder
                .bridge
                .charge_work((close - cursor).min(CANCELLATION_INTERVAL_BRIDGE_BYTES))?;
        }
        let byte = bytes[cursor];
        if consume_quote_state(byte, &mut state.quote, &mut state.escaped) {
            cursor += 1;
            continue;
        }
        if state.update_depth(byte, true) {
            cursor += 1;
            continue;
        }
        if byte == b';' && state.top_level() {
            return Ok(cursor + 1);
        }
        cursor += 1;
    }
    Ok(close)
}

fn scan_native_modules_calls(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let marker = "NativeModules.";
    let mut cursor = 0;
    while let Some(position) = bridge_marker(builder, source, (cursor, marker))? {
        let module_start = position + marker.len();
        let bounded = bounded_bridge_range(source_range(source, module_start));
        builder.bridge.charge_work(bounded.end - bounded.start)?;
        let Some((module_end, module)) = bridge_identifier(bounded) else {
            cursor = module_start;
            continue;
        };
        builder.add_reference_near(FrameworkNearReferenceInput {
            name: module,
            resolution_name: None,
            kind: ReferenceKind::References,
            start: module_start,
            end: module_end,
        })?;
        let method_start = skip_ascii_whitespace(&source[..bounded.end], module_end);
        if source.as_bytes()[..bounded.end].get(method_start) == Some(&b'.')
            && let Some((method_end, method)) = bridge_identifier(SymbolRange {
                start: method_start + 1,
                ..bounded
            })
            && !react_native_blocklisted(method)
        {
            builder.add_reference_near_with_resolution(FrameworkNearReferenceInput {
                name: method,
                resolution_name: Some(&format!("{module}::{method}")),
                kind: ReferenceKind::Calls,
                start: method_start + 1,
                end: method_end,
            })?;
        }
        cursor = module_end;
    }
    Ok(())
}

fn scan_registry_modules(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let mut seen_calls = BTreeSet::new();
    for marker in REGISTRY_FACTORIES {
        let mut cursor = 0;
        while let Some(start) = bridge_marker(builder, source, (cursor, marker))? {
            cursor = start + marker.len();
            let Some(module) = legacy_bridge_argument(builder, source, cursor)? else {
                continue;
            };
            cursor = module.end;
            if !seen_calls.insert(start) {
                continue;
            }
            builder.add_reference_near(FrameworkNearReferenceInput {
                name: module.value,
                resolution_name: None,
                kind: ReferenceKind::References,
                start: module.start,
                end: module.end,
            })?;
            add_landmark(
                builder,
                (SymbolKind::Resource, "native-module-spec"),
                (module.value, module.start, module.end),
            )?;
        }
    }
    Ok(())
}

fn scan_codegen_components(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    const MARKER: &str = "codegenNativeComponent";
    let mut cursor = 0;
    while let Some(start) = bridge_marker(builder, source, (cursor, MARKER))? {
        cursor = start + MARKER.len();
        let Some(component) = legacy_bridge_argument(builder, source, cursor)? else {
            continue;
        };
        add_landmark(
            builder,
            (SymbolKind::Component, "fabric-component"),
            (component.value, component.start, component.end),
        )?;
        cursor = component.end;
    }
    if bridge_marker(builder, source, (0, MARKER))?.is_none() {
        return Ok(());
    }
    let Some(interface) = bridge_marker(builder, source, (0, "NativeProps"))? else {
        return Ok(());
    };
    let Some((open, close)) = bridge_body_range(builder, source, interface)? else {
        return Ok(());
    };
    builder
        .bridge
        .charge_work((close - open).saturating_mul(2))?;
    for (offset, name) in declaration_names(&source[open + 1..close]) {
        add_landmark(
            builder,
            (SymbolKind::Property, "fabric-prop"),
            (name, open + 1 + offset, open + 1 + offset + name.len()),
        )?;
    }
    Ok(())
}

struct ObjcBridgeScopes<'source> {
    containers: Vec<SymbolRange<'source>>,
    registrations: BTreeMap<&'source str, Option<(String, usize, usize)>>,
    implementations: BTreeSet<&'source str>,
}

fn scan_objc(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    let scopes = objc_bridge_scopes(builder, source)?;
    for range in &scopes.containers {
        builder.check_cancelled()?;
        scan_objc_container(builder, *range, &scopes)?;
    }
    scan_native_event_producers(
        builder,
        source,
        &["sendEventWithName:", "sendEventWithName("],
    )?;
    scan_objc_swift_aliases(builder, source)
}

fn objc_bridge_scopes<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &'source str,
) -> Result<ObjcBridgeScopes<'source>, ExtractError> {
    let mut scopes = ObjcBridgeScopes {
        containers: Vec::new(),
        registrations: BTreeMap::new(),
        implementations: BTreeSet::new(),
    };
    let mut cursor = 0;
    while let Some((start, marker)) = next_objc_container_marker(builder, source, cursor)? {
        cursor = start + marker.len();
        if marker == "@end" {
            continue;
        }
        let Some((end, closing)) = next_objc_container_marker(builder, source, cursor)? else {
            break;
        };
        cursor = end;
        if closing != "@end" {
            continue;
        }
        cursor += closing.len();
        if marker == "@implementation" || marker == "@interface" {
            let range = SymbolRange {
                source,
                start,
                end: cursor,
            };
            let bytes =
                u64::try_from(OBJC_SCOPE_ENTRY_BYTES).map_err(|_| ExtractError::OutputLimit)?;
            builder.bridge.reserve_working_bytes(bytes)?;
            record_objc_registration(builder, &mut scopes, range)?;
            scopes.containers.push(range);
        }
    }
    Ok(scopes)
}

fn record_objc_registration<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    scopes: &mut ObjcBridgeScopes<'source>,
    range: SymbolRange<'source>,
) -> Result<(), ExtractError> {
    builder.check_cancelled()?;
    builder
        .bridge
        .charge_work((range.end - range.start).min(MAX_BRIDGE_SCAN_BYTES))?;
    let source = &range.source[range.start..range.end];
    let Some(class) = objc_class_name(source) else {
        return Ok(());
    };
    if source.starts_with("@implementation") {
        scopes.implementations.insert(class.0);
    }
    let class = (class.0, range.start + class.1, range.start + class.2);
    if let Some(registration) = objc_module_name(builder, range, Some(class))? {
        scopes
            .registrations
            .entry(class.0)
            .and_modify(|entry| {
                if entry
                    .as_ref()
                    .is_none_or(|previous| previous.0 != registration.0)
                {
                    *entry = None;
                }
            })
            .or_insert(Some(registration));
    }
    Ok(())
}

fn next_objc_container_marker(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    mut cursor: usize,
) -> Result<Option<(usize, &'static str)>, ExtractError> {
    use crate::objc_lex::{ObjcLexer, Region};

    let bytes = source.as_bytes();
    let mut lexer = ObjcLexer::default();
    while cursor < bytes.len() {
        let (region, next) = lexer.step(bytes, cursor);
        builder.bridge.charge_work(next - cursor)?;
        if region != Region::Code {
            cursor = next;
            continue;
        }
        if bytes[cursor] == b'@'
            && let Some(marker) = OBJC_CONTAINER_MARKERS.iter().find(|marker| {
                bytes[cursor..].starts_with(marker.as_bytes())
                    && bytes
                        .get(cursor + marker.len())
                        .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            })
            && (*marker != "@protocol" || objc_protocol_container(builder, source, cursor)?)
        {
            return Ok(Some((cursor, marker)));
        }
        cursor = next;
    }
    Ok(None)
}

#[derive(Clone, Copy)]
struct ObjcCodeMarker<'source, 'name> {
    range: SymbolRange<'source>,
    cursor: usize,
    name: &'name str,
}

fn next_objc_code_marker(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ObjcCodeMarker<'_, '_>,
) -> Result<Option<usize>, ExtractError> {
    use crate::objc_lex::{ObjcLexer, Region};
    let bytes = input.range.source.as_bytes();
    let mut lexer = ObjcLexer::default();
    let mut cursor = input.cursor;
    while cursor < input.range.end {
        let (region, next) = lexer.step(&bytes[..input.range.end], cursor);
        builder.bridge.charge_work(next - cursor)?;
        let boundary = cursor
            .checked_sub(1)
            .and_then(|before| bytes.get(before))
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_');
        if region == Region::Code
            && boundary
            && bytes[cursor..input.range.end].starts_with(input.name.as_bytes())
        {
            return Ok(Some(cursor));
        }
        cursor = next;
    }
    Ok(None)
}

fn objc_protocol_container(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    start: usize,
) -> Result<bool, ExtractError> {
    let range = bounded_bridge_range(source_range(source, start + "@protocol".len()));
    builder.bridge.charge_work(range.end - range.start)?;
    let source = &source[..range.end];
    let name_start = skip_ascii_whitespace(source, range.start);
    Ok(bridge_identifier(SymbolRange {
        start: name_start,
        ..range
    })
    .is_some_and(|(end, _)| {
        !matches!(
            source.as_bytes().get(skip_ascii_whitespace(source, end)),
            Some(b';' | b',')
        )
    }))
}

fn scan_objc_container(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'_>,
    scopes: &ObjcBridgeScopes<'_>,
) -> Result<(), ExtractError> {
    let source = &range.source[range.start..range.end];
    builder
        .bridge
        .charge_work(source.len().min(MAX_BRIDGE_SCAN_BYTES))?;
    let class = objc_class_name(source);
    let module = class
        .and_then(|class| scopes.registrations.get(class.0))
        .and_then(Option::as_ref);
    if let Some((name, start, end)) = module
        && range.start <= *start
        && *end <= range.end
    {
        add_landmark(
            builder,
            (SymbolKind::Resource, "react-native-module"),
            (name, *start, *end),
        )?;
    }
    if let Some((module_name, _, _)) = module {
        for marker in [
            "RCT_EXPORT_METHOD(",
            "RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(",
            "RCT_EXTERN_METHOD(",
            "RCT_EXTERN__BLOCKING_SYNCHRONOUS_METHOD(",
            "RCT_REMAP_METHOD(",
            "RCT_EXTERN_REMAP_METHOD(",
            "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(",
        ] {
            scan_objc_method_macro(builder, range, (marker, module_name))?;
        }
    }
    if source.starts_with("@implementation")
        || class.is_some_and(|(name, _, _)| !scopes.implementations.contains(name))
    {
        scan_native_view_manager(
            builder,
            range,
            class.map(|(name, start, end)| (name, range.start + start, range.start + end)),
        )?;
    }
    Ok(())
}

fn scan_objc_method_macro(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'_>,
    (marker, module): (&str, &str),
) -> Result<(), ExtractError> {
    let source = &range.source[..range.end];
    let mut cursor = range.start;
    while let Some(position) = next_objc_code_marker(
        builder,
        ObjcCodeMarker {
            range,
            cursor,
            name: marker,
        },
    )? {
        let start = position + marker.len();
        let bounded = bounded_bridge_range(SymbolRange { start, ..range });
        let first = all_identifiers(&source[start..bounded.end]).next();
        let inspected = first.map_or(bounded.end - start, |(offset, name)| offset + name.len());
        builder.bridge.charge_work(inspected.saturating_mul(2))?;
        let Some((offset, _)) = first else {
            cursor = start;
            continue;
        };
        let name_start = start + offset;
        let Some((name_end, name)) = bridge_identifier(SymbolRange {
            start: name_start,
            ..bounded
        }) else {
            cursor = bounded.end;
            continue;
        };
        if react_native_blocklisted(name) {
            cursor = name_end;
            continue;
        }
        add_member_landmark(
            builder,
            (SymbolKind::Method, "react-native-method", module),
            (name, name_start, name_end),
        )?;
        cursor = name_end;
    }
    Ok(())
}

fn scan_native_view_manager(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'_>,
    class: Option<(&str, usize, usize)>,
) -> Result<(), ExtractError> {
    let Some((class, start, end)) =
        class.filter(|(name, _, _)| name.ends_with("ViewManager") || name.ends_with("Manager"))
    else {
        return Ok(());
    };
    let component = derive_component_name(class);
    add_landmark(
        builder,
        (SymbolKind::Component, "native-view-manager"),
        (&component, start, end),
    )?;
    for marker in ["RCT_EXPORT_VIEW_PROPERTY(", "RCT_REMAP_VIEW_PROPERTY("] {
        let mut cursor = range.start;
        while let Some(position) = next_objc_code_marker(
            builder,
            ObjcCodeMarker {
                range,
                cursor,
                name: marker,
            },
        )? {
            let property_start = position + marker.len();
            let bounded = bounded_bridge_range(SymbolRange {
                start: property_start,
                ..range
            });
            let property = bridge_identifier(bounded);
            builder
                .bridge
                .charge_work(property.map_or(bounded.end, |(end, _)| end) - property_start)?;
            let Some((property_end, property)) = property else {
                cursor = property_start;
                continue;
            };
            add_member_landmark(
                builder,
                (SymbolKind::Property, "native-view-prop", &component),
                (property, property_start, property_end),
            )?;
            cursor = property_end;
        }
    }
    Ok(())
}

fn scan_jvm(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    ownership: &BridgeOwnership<'_>,
) -> Result<(), ExtractError> {
    for &index in &ownership.classes {
        builder.bridge.charge_work(1)?;
        if ownership.range(index).is_none() {
            continue;
        }
        if let Some((module, start, end)) = react_native_jvm_module(builder, ownership, index)? {
            add_landmark(
                builder,
                (SymbolKind::Resource, "react-native-module"),
                (&module, start, end),
            )?;
            scan_react_methods(builder, ownership, (index, &module))?;
        }
        scan_jvm_view_manager(builder, ownership, index)?;
    }
    scan_expo_module(builder, ownership)?;
    for marker in [
        "RCTDeviceEventEmitter",
        "DeviceEventManagerModule",
        "expo.modules",
    ] {
        if bridge_marker(builder, source, (0, marker))?.is_some() {
            scan_native_event_producers(builder, source, &[".emit(", "sendEvent("])?;
            break;
        }
    }
    Ok(())
}

fn react_native_jvm_module(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'_>,
    class: usize,
) -> Result<Option<(String, usize, usize)>, ExtractError> {
    let annotations = ownership.annotations.get(&(class, "ReactModule"));
    if annotations.is_none() && !ownership.annotations.contains_key(&(class, "ReactMethod")) {
        return Ok(None);
    }
    if let Some(site) = annotations.and_then(|sites| sites.first())
        && let Some(module) = jvm_annotation_argument(builder, ownership, *site)?
    {
        return Ok(Some((module.value.to_owned(), module.start, module.end)));
    }
    if let Some(module) = react_native_jvm_name_method(builder, ownership, class)? {
        return Ok(Some((module.value.to_owned(), module.start, module.end)));
    }
    Ok(ownership
        .native_symbol_name(builder, class)?
        .filter(|(name, _, _)| name.ends_with("Module"))
        .map(|(class, start, end)| {
            (
                class.strip_suffix("Module").unwrap_or(class).to_owned(),
                start,
                end,
            )
        }))
}

fn jvm_annotation_argument<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'source>,
    site: AnnotationSite,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let Some(range) = ownership.range(site.symbol) else {
        return Ok(None);
    };
    let Some(body) = bridge_argument_body(
        builder,
        SymbolRange {
            start: site.end,
            ..range
        },
    )?
    else {
        return Ok(None);
    };
    Ok(quoted_after(&body.source[..body.end], body.start))
}

fn react_native_jvm_name_method<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'source>,
    class: usize,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    for &index in ownership.members.get(&class).into_iter().flatten() {
        builder.bridge.charge_work(1)?;
        let is_name = builder.original_symbol(index).is_some_and(|symbol| {
            symbol.name == "getName"
                && matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function)
        });
        if !is_name {
            continue;
        }
        let Some(range) = ownership.range(index).map(bounded_bridge_range) else {
            continue;
        };
        let Some(method) = builder.syntax_root().and_then(|root| {
            ownership
                .range(index)
                .and_then(|full| root.named_descendant_for_byte_range(full.start, full.end))
        }) else {
            continue;
        };
        if let Some(name) = jvm_method_module_name(builder, range, method)? {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn jvm_method_module_name<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
    method: tree_sitter::Node<'_>,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let mut cursor = method.walk();
    for child in method.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if child.start_byte() >= range.end {
            break;
        }
        if matches!(child.kind(), "block" | "function_body") {
            if let Some(name) = jvm_body_module_name(builder, range, child)? {
                return Ok(Some(name));
            }
            break;
        }
    }
    Ok(None)
}

fn jvm_body_module_name<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
    node: tree_sitter::Node<'_>,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    builder.bridge.charge_work(1)?;
    // A return in a nested declaration or lambda belongs to that callable.
    let nested = matches!(
        node.kind(),
        "class_declaration"
            | "class_body"
            | "object_declaration"
            | "object_literal"
            | "method_declaration"
            | "constructor_declaration"
            | "function_declaration"
            | "anonymous_function"
            | "lambda_literal"
            | "lambda_expression"
    );
    if nested || node.start_byte() >= range.end {
        return Ok(None);
    }
    let value = match node.kind() {
        "return_statement" | "function_body" => jvm_expression_value(builder, node, range.end)?,
        "jump_expression" if node.child(0).is_some_and(|child| child.kind() == "return") => {
            jvm_expression_value(builder, node, range.end)?
        }
        _ => None,
    };
    if let Some(value) = value.filter(|value| value.kind() == "string_literal")
        && value.end_byte() <= range.end
    {
        builder
            .bridge
            .charge_work(value.end_byte() - value.start_byte())?;
        let literal = quoted_after(&range.source[..value.end_byte()], value.start_byte());
        return Ok(literal.filter(|literal| literal.quote_end + 1 == value.end_byte()));
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name) = jvm_body_module_name(builder, range, child)? {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn jvm_expression_value<'tree>(
    builder: &mut FrameworkBuilder<'_, '_>,
    node: tree_sitter::Node<'tree>,
    limit: usize,
) -> Result<Option<tree_sitter::Node<'tree>>, ExtractError> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if child.start_byte() >= limit {
            break;
        }
        if !child.is_extra() {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

fn scan_react_methods(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'_>,
    (class, module): (usize, &str),
) -> Result<(), ExtractError> {
    let Some(sites) = ownership.annotations.get(&(class, "ReactMethod")) else {
        return Ok(());
    };
    for site in sites {
        builder.bridge.charge_work(1)?;
        let Some((method, start, end)) = ownership.native_symbol_name(builder, site.symbol)? else {
            continue;
        };
        if !react_native_blocklisted(method) {
            add_member_landmark(
                builder,
                (SymbolKind::Method, "react-native-method", module),
                (method, start, end),
            )?;
        }
    }
    Ok(())
}

fn scan_jvm_view_manager(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'_>,
    class: usize,
) -> Result<(), ExtractError> {
    let candidate = builder.original_symbol(class).is_some_and(|symbol| {
        symbol.name.ends_with("ViewManager")
            || (symbol.name.ends_with("Manager") && ownership.view_classes.contains(&class))
    });
    if !candidate {
        return Ok(());
    }
    let Some((name, start, end)) = ownership.native_symbol_name(builder, class)? else {
        return Ok(());
    };
    let component = derive_component_name(name);
    add_landmark(
        builder,
        (SymbolKind::Component, "native-view-manager"),
        (&component, start, end),
    )?;
    for marker in ["ReactProp", "ReactPropGroup"] {
        for site in ownership
            .annotations
            .get(&(class, marker))
            .into_iter()
            .flatten()
        {
            builder.bridge.charge_work(1)?;
            let Some(property) = jvm_annotation_argument(builder, ownership, *site)? else {
                continue;
            };
            add_member_landmark(
                builder,
                (SymbolKind::Property, "native-view-prop", &component),
                (property.value, property.start, property.end),
            )?;
        }
    }
    Ok(())
}

fn scan_swift(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    ownership: &BridgeOwnership<'_>,
) -> Result<(), ExtractError> {
    scan_expo_module(builder, ownership)?;
    scan_swift_objc_exports(builder, source, ownership)?;
    scan_native_event_producers(builder, source, &["sendEvent(withName:", "sendEvent("])
}

/// One Objective-C method the Swift-name aliases are derived from.
struct ObjcAliasSource {
    selector: String,
    start: usize,
    end: usize,
    declaration_only: bool,
}

/// Swift-visible base names for every Objective-C method. Methods are named by
/// their full selector; an alias of a header declaration stays declaration-only
/// so a same-selector implementation is the unique bridge target, and React
/// Native export-macro methods keep only their JS bridge landmark.
fn scan_objc_swift_aliases(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    for index in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        let Some(method) = objc_alias_source(builder, index) else {
            continue;
        };
        builder.bridge.charge_work(
            (method.end - method.start)
                .min(MAX_BRIDGE_SCAN_BYTES)
                .saturating_add(method.selector.len().saturating_mul(10)),
        )?;
        if source
            .get(method.start..)
            .is_some_and(|declaration| declaration.starts_with(REACT_NATIVE_MACRO_PREFIX))
        {
            continue;
        }
        for alias in swift_base_names_for_objc_selector(&method.selector) {
            if apple_bridge_generic_name(&alias) {
                continue;
            }
            let landmark = add_landmark_with_id(
                builder,
                (
                    SymbolKind::Method,
                    &format!("objc-swift-method::{}", method.selector),
                ),
                (&alias, method.start, method.end),
            )?;
            if method.declaration_only
                && let Some(landmark) = landmark
            {
                builder.mark_landmark_declaration_only(&landmark);
            }
        }
    }
    Ok(())
}

fn objc_alias_source(builder: &FrameworkBuilder<'_, '_>, index: usize) -> Option<ObjcAliasSource> {
    let symbol = builder
        .original_symbol(index)
        .filter(|symbol| symbol.kind == SymbolKind::Method)?;
    Some(ObjcAliasSource {
        selector: symbol.name.clone(),
        start: usize::try_from(symbol.span.start_byte()).ok()?,
        end: usize::try_from(symbol.span.end_byte()).ok()?,
        declaration_only: symbol.implementation.declaration_only,
    })
}

fn scan_swift_objc_exports(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    ownership: &BridgeOwnership<'_>,
) -> Result<(), ExtractError> {
    if bridge_marker(builder, source, (0, "@objc"))?.is_none() {
        return Ok(());
    }
    for index in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        let Some((name, start, end)) = builder.original_symbol(index).and_then(|symbol| {
            matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function)
                .then(|| {
                    Some((
                        symbol.name.clone(),
                        usize::try_from(symbol.span.start_byte()).ok()?,
                        usize::try_from(symbol.span.end_byte()).ok()?,
                    ))
                })
                .flatten()
        }) else {
            continue;
        };
        let range = SymbolRange { source, start, end };
        if symbol_has_swift_attribute(
            builder,
            NamedSymbolRange {
                range,
                name: "nonobjc",
            },
        )? {
            continue;
        }
        let explicit = symbol_has_swift_attribute(
            builder,
            NamedSymbolRange {
                range,
                name: "objc",
            },
        )?;
        if !explicit && !containing_objc_members_class(builder, ownership, index)? {
            continue;
        }
        builder
            .bridge
            .charge_work((end - start).min(MAX_BRIDGE_SCAN_BYTES))?;
        let (span_start, span_end) = symbol_name_span(NamedSymbolRange { range, name: &name });
        let mut aliases = BTreeSet::from([name.clone()]);
        if let Some(selector) = swift_objc_selector_attribute(builder, range)? {
            aliases.extend(swift_base_names_for_objc_selector(&selector));
        }
        for alias in aliases {
            add_member_landmark(
                builder,
                (SymbolKind::Method, "swift-objc-method", &name),
                (&alias, span_start, span_end),
            )?;
        }
    }
    Ok(())
}

// Preserve the HEAD text-attribute rule and lexical containment, using the
// already indexed native class relationship instead of rescanning all symbols.
fn containing_objc_members_class(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'_>,
    member: usize,
) -> Result<bool, ExtractError> {
    builder.bridge.charge_work(1)?;
    let Some((class, member)) = ownership
        .owner(member)
        .and_then(|class| ownership.range(class).zip(ownership.range(member)))
    else {
        return Ok(false);
    };
    if class.start > member.start || member.end > class.end {
        return Ok(false);
    }
    symbol_has_swift_attribute(
        builder,
        NamedSymbolRange {
            range: class,
            name: "objcMembers",
        },
    )
}

fn symbol_has_swift_attribute(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: NamedSymbolRange<'_, '_>,
) -> Result<bool, ExtractError> {
    Ok(swift_symbol_attribute_text(builder, input.range)?
        .into_iter()
        .any(|text| contains_swift_attribute(text, input.name)))
}

fn swift_objc_selector_attribute(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'_>,
) -> Result<Option<String>, ExtractError> {
    for text in swift_symbol_attribute_text(builder, range)? {
        let marker = "@objc(";
        let Some(open) = text.find(marker).map(|offset| offset + marker.len()) else {
            continue;
        };
        let Some(close) = text[open..].find(')').map(|offset| open + offset) else {
            return Ok(None);
        };
        let selector = text[open..close].trim();
        if !selector.is_empty()
            && selector.len() <= MAX_SWIFT_SELECTOR_BYTES
            && selector
                .bytes()
                .all(|byte| byte == b':' || byte == b'_' || byte.is_ascii_alphanumeric())
        {
            builder.bridge.reserve_working_bytes(
                u64::try_from(selector.len()).map_err(|_| ExtractError::OutputLimit)?,
            )?;
            return Ok(Some(selector.to_owned()));
        }
    }
    Ok(None)
}

fn swift_symbol_attribute_text<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
) -> Result<[&'source str; 2], ExtractError> {
    let bounded = bounded_bridge_range(range);
    let prefix = bounded_bridge_prefix(range.source, range.start);
    builder
        .bridge
        .charge_work((bounded.end - bounded.start).saturating_mul(2))?;
    let mut lines = prefix.rsplit('\n');
    let current = lines.next().unwrap_or_default();
    builder
        .bridge
        .charge_work(current.len().saturating_mul(2))?;
    let mut prefix_start = prefix.len() - current.len();
    for line in lines.take(MAX_SWIFT_ATTRIBUTE_LINES) {
        builder
            .bridge
            .charge_work(line.len().saturating_mul(2).saturating_add(1))?;
        if !line.trim().starts_with('@') {
            break;
        }
        prefix_start = prefix_start.saturating_sub(line.len() + 1);
    }
    Ok([
        &prefix[prefix_start..],
        &range.source[range.start..bounded.end],
    ])
}

fn contains_swift_attribute(value: &str, name: &str) -> bool {
    let marker = format!("@{name}");
    let mut cursor = 0;
    while let Some(relative) = value[cursor..].find(&marker) {
        let end = cursor + relative + marker.len();
        if value
            .as_bytes()
            .get(end)
            .is_none_or(|byte| !(*byte == b'_' || byte.is_ascii_alphanumeric()))
        {
            return true;
        }
        cursor = end;
    }
    false
}

fn symbol_name_span(input: NamedSymbolRange<'_, '_>) -> (usize, usize) {
    let bounded_end = input
        .range
        .end
        .min(input.range.start.saturating_add(MAX_BRIDGE_SCAN_BYTES));
    input
        .range
        .source
        .get(input.range.start..bounded_end)
        .and_then(|value| value.find(input.name))
        .map_or((input.range.start, input.range.end), |offset| {
            (
                input.range.start + offset,
                input.range.start + offset + input.name.len(),
            )
        })
}

fn swift_base_names_for_objc_selector(selector: &str) -> BTreeSet<String> {
    let raw = selector.rsplit('.').next().unwrap_or(selector);
    let without_trailing = raw.trim_end_matches(':');
    let first = without_trailing.split(':').next().unwrap_or_default();
    let mut candidates = BTreeSet::new();
    if first.is_empty() {
        return candidates;
    }
    candidates.insert(first.to_owned());
    if first.starts_with("initWith") {
        candidates.insert("init".to_owned());
    }
    for preposition in [
        "With", "For", "By", "In", "On", "At", "From", "To", "Of", "As",
    ] {
        if let Some(index) = first.find(preposition)
            && index > 0
            && first
                .as_bytes()
                .get(index + preposition.len())
                .is_some_and(u8::is_ascii_uppercase)
            && first.as_bytes()[0].is_ascii_lowercase()
        {
            candidates.insert(first[..index].to_owned());
        }
    }
    if !without_trailing.contains(':')
        && raw.ends_with(':')
        && first.starts_with("set")
        && first.as_bytes().get(3).is_some_and(u8::is_ascii_uppercase)
    {
        let property = &first[3..];
        if let Some(first_byte) = property.as_bytes().first() {
            let mut lowered = String::with_capacity(property.len());
            lowered.push(char::from(first_byte.to_ascii_lowercase()));
            lowered.push_str(&property[1..]);
            candidates.insert(lowered);
        }
    }
    candidates
}

const APPLE_BRIDGE_GENERIC_NAMES: &[&str] = &[
    "init",
    "description",
    "debugDescription",
    "hash",
    "isEqual",
    "copy",
    "mutableCopy",
    "class",
    "self",
    "count",
    "length",
    "value",
    "name",
    "data",
    "string",
    "object",
    "load",
    "save",
    "dealloc",
    "release",
    "retain",
    "autorelease",
];

fn apple_bridge_generic_name(name: &str) -> bool {
    APPLE_BRIDGE_GENERIC_NAMES.contains(&name)
}

fn bridge_marker(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    query: (usize, &str),
) -> Result<Option<usize>, ExtractError> {
    let (start, marker) = query;
    let mut range = SymbolRange {
        source,
        start,
        end: source.len(),
    };
    while range.start < range.end {
        builder.bridge.charge_work(0)?;
        let window = bounded_bridge_range(range);
        if let Some(relative) = range.source[window.start..window.end].find(marker) {
            builder.bridge.charge_work(relative + marker.len())?;
            return Ok(Some(window.start + relative));
        }
        builder.bridge.charge_work(window.end - window.start)?;
        if window.end == range.end {
            break;
        }
        range.start = window.end.saturating_sub(marker.len().saturating_sub(1));
        while !range.source.is_char_boundary(range.start) {
            range.start += 1;
        }
    }
    Ok(None)
}

fn source_range(source: &str, start: usize) -> SymbolRange<'_> {
    SymbolRange {
        source,
        start,
        end: source.len(),
    }
}

fn bounded_bridge_range(range: SymbolRange<'_>) -> SymbolRange<'_> {
    let mut end = range
        .end
        .min(range.start.saturating_add(MAX_BRIDGE_SCAN_BYTES));
    while !range.source.is_char_boundary(end) {
        end -= 1;
    }
    SymbolRange { end, ..range }
}

fn scan_expo_module(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'_>,
) -> Result<(), ExtractError> {
    let mut registered = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for definition in &ownership.definitions {
        builder.bridge.charge_work(1)?;
        if let Some(class) = ownership.owner(*definition)
            && ownership.expo_classes.contains(&class)
        {
            registered.insert(class);
        }
    }
    for definition in &ownership.definitions {
        builder.bridge.charge_work(1)?;
        if ownership
            .owner(*definition)
            .is_some_and(|class| registered.contains(&class))
        {
            scan_expo_definition(
                builder,
                ownership,
                &mut ExpoDefinitionScan {
                    definition: *definition,
                    include_outside: registered.len() == 1,
                    seen: &mut seen,
                },
            )?;
        }
    }
    Ok(())
}

fn scan_expo_definition<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'source>,
    scope: &mut ExpoDefinitionScan<'_, 'source>,
) -> Result<(), ExtractError> {
    let calls = ownership
        .calls
        .get(&Some(scope.definition))
        .map_or(&[][..], Vec::as_slice);
    let outside = if scope.include_outside {
        ownership.calls.get(&None).map_or(&[][..], Vec::as_slice)
    } else {
        &[]
    };
    builder.bridge.charge_work(calls.len())?;
    let name_calls = if calls.iter().any(|call| call.name == "Name") {
        calls
    } else {
        outside
    };
    let mut module = None;
    for call in name_calls.iter().filter(|call| call.name == "Name") {
        builder.bridge.charge_work(1)?;
        let Some(name) = expo_call_argument(builder, ownership, *call)? else {
            return Ok(());
        };
        if module.is_some_and(|(value, _, _)| value != name.value) {
            return Ok(());
        }
        module = Some((name.value, name.start, name.end));
    }
    // An unreadable explicit Name never becomes a guessed class-name export.
    if module.is_none()
        && let Some(class) = ownership.owner(scope.definition)
    {
        module = ownership.native_symbol_name(builder, class)?;
    }
    let Some((module, start, end)) = module else {
        return Ok(());
    };
    add_landmark(
        builder,
        (SymbolKind::Resource, "expo-module"),
        (module, start, end),
    )?;
    for call in calls
        .iter()
        .chain(outside)
        .filter(|call| call.name != "Name")
    {
        builder.bridge.charge_work(1)?;
        let Some(member) = expo_call_argument(builder, ownership, *call)? else {
            continue;
        };
        let Some(class) = ownership.owner(scope.definition) else {
            continue;
        };
        if !react_native_blocklisted(member.value)
            && scope.seen.insert((class, module, member.value))
        {
            builder
                .bridge
                .reserve_working_bytes(EXPO_EXPORT_ENTRY_BYTES)?;
            add_member_landmark(
                builder,
                (SymbolKind::Method, "expo-module-method", module),
                (member.value, member.start, member.end),
            )?;
        }
    }
    Ok(())
}

fn expo_call_argument<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    ownership: &BridgeOwnership<'source>,
    call: BridgeCall,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    if !call.supported {
        return Ok(None);
    }
    let argument = quoted_bridge_argument(
        builder,
        SymbolRange {
            source: ownership.source,
            start: call.end,
            end: call.limit,
        },
    )?;
    if let Some(argument) = argument.as_ref() {
        builder
            .bridge
            .charge_work(argument.value.len().saturating_mul(2))?;
    }
    Ok(argument
        .filter(|argument| static_expo_literal(builder.language(), ownership.source, argument)))
}

fn static_expo_literal(language: SourceLanguage, source: &str, argument: &Quoted<'_>) -> bool {
    if source.as_bytes().get(argument.start.saturating_sub(1)) != Some(&b'"')
        || argument.value.contains('\\')
        || argument.value.is_empty()
    {
        return false;
    }
    if language != SourceLanguage::Kotlin {
        return true;
    }
    let mut characters = argument.value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '$'
            && characters
                .peek()
                .is_some_and(|next| matches!(next, '{' | '_' | '`') || next.is_alphabetic())
        {
            return false;
        }
    }
    true
}

fn quoted_bridge_argument<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
) -> Result<Option<Quoted<'source>>, ExtractError> {
    let Some(body) = bridge_argument_body(builder, range)? else {
        return Ok(None);
    };
    let source = &body.source[..body.end];
    let Some(argument) = quoted_event_after(source, body.start) else {
        return Ok(None);
    };
    let after = skip_ascii_whitespace(body.source, argument.quote_end + 1);
    Ok((after == body.end || source.as_bytes().get(after) == Some(&b',')).then_some(argument))
}

fn bridge_argument_body<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
) -> Result<Option<SymbolRange<'source>>, ExtractError> {
    let range = bounded_bridge_range(range);
    let open = skip_ascii_whitespace(&range.source[..range.end], range.start);
    builder.bridge.charge_work(open - range.start)?;
    let Some(close) = bridge_argument_close(
        SymbolRange {
            start: open,
            ..range
        },
        &mut |units| builder.bridge.charge_work(units),
    )?
    else {
        return Ok(None);
    };
    builder.bridge.charge_work(close - open)?;
    Ok(Some(SymbolRange {
        start: open + 1,
        end: close,
        ..range
    }))
}

fn bridge_argument_close(
    range: SymbolRange<'_>,
    poll: &mut dyn FnMut(usize) -> Result<(), ExtractError>,
) -> Result<Option<usize>, ExtractError> {
    bridge_delimiter_close(range, (b'(', b')'), poll)
}

fn bridge_delimiter_close(
    range: SymbolRange<'_>,
    delimiters: (u8, u8),
    poll: &mut dyn FnMut(usize) -> Result<(), ExtractError>,
) -> Result<Option<usize>, ExtractError> {
    let range = bounded_bridge_range(range);
    if range.source.as_bytes().get(range.start) != Some(&delimiters.0) {
        return Ok(None);
    }
    let mut state = BridgeDelimiterState::default();
    let mut depth = 0_usize;
    let mut pending = 0;
    poll(0)?;
    for position in range.start..range.end {
        if pending == CANCELLATION_INTERVAL_BRIDGE_BYTES {
            poll(pending)?;
            pending = 0;
        }
        pending += 1;
        let byte = range.source.as_bytes()[position];
        if consume_quote_state(byte, &mut state.quote, &mut state.escaped) {
            continue;
        }
        match byte {
            byte if byte == delimiters.0 => depth += 1,
            byte if byte == delimiters.1 => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    poll(pending)?;
                    return Ok(Some(position));
                }
            }
            _ => {}
        }
    }
    poll(pending)?;
    Ok(None)
}

fn add_member_landmark(
    builder: &mut FrameworkBuilder<'_, '_>,
    (kind, category, module): (SymbolKind, &str, &str),
    site: (&str, usize, usize),
) -> Result<(), ExtractError> {
    add_landmark(builder, (kind, &format!("{category}::{module}")), site)
}

fn add_landmark(
    builder: &mut FrameworkBuilder<'_, '_>,
    classification: (SymbolKind, &str),
    site: (&str, usize, usize),
) -> Result<(), ExtractError> {
    add_landmark_with_id(builder, classification, site).map(|_| ())
}

fn add_landmark_with_id(
    builder: &mut FrameworkBuilder<'_, '_>,
    (kind, category): (SymbolKind, &str),
    (name, start, end): (&str, usize, usize),
) -> Result<Option<SymbolId>, ExtractError> {
    if name.is_empty() || start >= end || end > builder.source().len() {
        return Ok(None);
    }
    builder.add_landmark_with_id(LandmarkInput {
        kind,
        name: name.to_owned(),
        identity: format!("{category}::{name}"),
        start,
        end,
        body_search_text: format!("{category} {name}"),
        target: None,
    })
}

enum ObjcModuleRegistration<'source> {
    Absent,
    DefaultClass,
    Invalid,
    Named(&'source str, (usize, usize)),
}

fn objc_module_name(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'_>,
    class: Option<(&str, usize, usize)>,
) -> Result<Option<(String, usize, usize)>, ExtractError> {
    let mut saw_export_module = false;
    for marker in [
        "RCT_EXTERN_REMAP_MODULE(",
        "RCT_EXTERN_MODULE(",
        "RCT_EXPORT_MODULE(",
    ] {
        match objc_module_registration(builder, range, marker)? {
            ObjcModuleRegistration::Absent => {}
            ObjcModuleRegistration::DefaultClass => saw_export_module = true,
            ObjcModuleRegistration::Invalid => return Ok(None),
            ObjcModuleRegistration::Named(name, span) => {
                return owned_objc_module_name(builder, name, span).map(Some);
            }
        }
    }
    default_objc_module_name(builder, saw_export_module.then_some(class).flatten())
}

fn objc_module_registration<'source>(
    builder: &mut FrameworkBuilder<'_, '_>,
    range: SymbolRange<'source>,
    marker: &str,
) -> Result<ObjcModuleRegistration<'source>, ExtractError> {
    let Some(position) = next_objc_code_marker(
        builder,
        ObjcCodeMarker {
            range,
            cursor: range.start,
            name: marker,
        },
    )?
    else {
        return Ok(ObjcModuleRegistration::Absent);
    };
    let start = position + marker.len();
    let bounded = bounded_bridge_range(SymbolRange { start, ..range });
    let source = &range.source[..bounded.end];
    if marker == "RCT_EXTERN_MODULE(" {
        return Ok(ObjcModuleRegistration::DefaultClass);
    }
    let name_start = skip_ascii_whitespace(source, start);
    let name = bridge_identifier(SymbolRange {
        start: name_start,
        ..bounded
    });
    let inspected = name.map_or(bounded.end, |(end, _)| end);
    builder.bridge.charge_work(inspected - start)?;
    if let Some((end, name)) = name
        && name != "RCT_EXPORT_MODULE"
    {
        return Ok(ObjcModuleRegistration::Named(name, (end - name.len(), end)));
    }
    if name.is_none() && !matches!(source.as_bytes().get(name_start), Some(b')' | b',')) {
        return Ok(ObjcModuleRegistration::Invalid);
    }
    Ok(ObjcModuleRegistration::DefaultClass)
}

fn default_objc_module_name(
    builder: &mut FrameworkBuilder<'_, '_>,
    class: Option<(&str, usize, usize)>,
) -> Result<Option<(String, usize, usize)>, ExtractError> {
    let Some((name, start, end)) = class else {
        return Ok(None);
    };
    let stripped = name
        .strip_prefix("RCT")
        .filter(|value| !value.is_empty())
        .unwrap_or(name);
    owned_objc_module_name(builder, stripped, (start, end)).map(Some)
}

fn owned_objc_module_name(
    builder: &mut FrameworkBuilder<'_, '_>,
    name: &str,
    span: (usize, usize),
) -> Result<(String, usize, usize), ExtractError> {
    let bytes = u64::try_from(name.len()).map_err(|_| ExtractError::OutputLimit)?;
    builder.bridge.reserve_working_bytes(bytes)?;
    Ok((name.to_owned(), span.0, span.1))
}

fn objc_class_name(source: &str) -> Option<(&str, usize, usize)> {
    let range = bounded_bridge_range(source_range(source, 0));
    for marker in ["@implementation", "@interface"] {
        let Some(start) = source.strip_prefix(marker).map(|_| marker.len()) else {
            continue;
        };
        let start = skip_ascii_whitespace(&source[..range.end], start);
        let (end, name) = bridge_identifier(SymbolRange { start, ..range })?;
        if matches!(name, "RCT_EXTERN_MODULE" | "RCT_EXTERN_REMAP_MODULE") {
            return objc_extern_class_name(
                SymbolRange {
                    start: end,
                    ..range
                },
                name,
            );
        }
        return Some((name, start, end));
    }
    None
}

fn objc_extern_class_name<'source>(
    range: SymbolRange<'source>,
    marker: &str,
) -> Option<(&'source str, usize, usize)> {
    let source = &range.source[..range.end];
    let open = skip_ascii_whitespace(source, range.start);
    if source.as_bytes().get(open) != Some(&b'(') {
        return None;
    }
    let first = skip_ascii_whitespace(source, open + 1);
    if marker == "RCT_EXTERN_MODULE" {
        return bridge_identifier(SymbolRange {
            start: first,
            ..range
        })
        .map(|(end, name)| (name, first, end));
    }
    let end = bridge_identifier(SymbolRange {
        start: first,
        ..range
    })
    .map_or(first, |(end, _)| end);
    let comma = skip_ascii_whitespace(source, end);
    if source.as_bytes().get(comma) != Some(&b',') {
        return None;
    }
    let start = skip_ascii_whitespace(source, comma + 1);
    bridge_identifier(SymbolRange { start, ..range }).map(|(end, name)| (name, start, end))
}

fn derive_component_name(class: &str) -> String {
    let stripped = class.strip_prefix("RCT").unwrap_or(class);
    stripped
        .strip_suffix("ViewManager")
        .or_else(|| stripped.strip_suffix("Manager"))
        .unwrap_or(stripped)
        .to_owned()
}

fn declaration_names(value: &str) -> Vec<(usize, &str)> {
    value
        .split_inclusive(['\n', ';'])
        .scan(0_usize, |offset, line| {
            let start = *offset;
            *offset = offset.saturating_add(line.len());
            Some((start, line))
        })
        .filter_map(|(start, line)| {
            let before_colon = line.split(':').next().unwrap_or(line);
            let (offset, name) = all_identifiers(before_colon)
                .filter(|(_, name)| {
                    !matches!(
                        *name,
                        "readonly" | "export" | "extends" | "interface" | "optional"
                    )
                })
                .last()?;
            Some((start + offset, name))
        })
        .collect()
}

fn bridge_identifier(range: SymbolRange<'_>) -> Option<(usize, &str)> {
    let range = bounded_bridge_range(range);
    let (end, name) = identifier_at(&range.source[..range.end], range.start)?;
    let continuing = range.source.as_bytes().get(end).is_some_and(|byte| {
        *byte == b'_' || *byte == b'$' || byte.is_ascii_alphanumeric() || !byte.is_ascii()
    });
    (!continuing).then_some((end, name))
}

fn all_identifiers(value: &str) -> impl Iterator<Item = (usize, &str)> {
    let bytes = value.as_bytes();
    let mut cursor = 0_usize;
    std::iter::from_fn(move || {
        while cursor < bytes.len()
            && !(bytes[cursor] == b'_'
                || bytes[cursor] == b'$'
                || bytes[cursor].is_ascii_alphabetic())
        {
            cursor += 1;
        }
        let start = cursor;
        if start == bytes.len() {
            return None;
        }
        cursor += 1;
        while cursor < bytes.len()
            && (bytes[cursor] == b'_'
                || bytes[cursor] == b'$'
                || bytes[cursor].is_ascii_alphanumeric())
        {
            cursor += 1;
        }
        Some((start, &value[start..cursor]))
    })
}

fn react_native_blocklisted(name: &str) -> bool {
    matches!(
        name,
        "addListener" | "removeListener" | "removeListeners" | "supportedEvents"
    )
}

#[cfg(test)]
mod bounded_scan_tests {
    use super::*;

    #[test]
    fn bridge_delimiter_scans_poll_cancellation_inside_the_argument() {
        const CANCEL_AT_POLL: usize = 4;
        let source = format!("({})", " ".repeat(MAX_BRIDGE_SCAN_BYTES));
        let range = SymbolRange {
            source: &source,
            start: 0,
            end: source.len(),
        };
        let mut polls = 0;
        let outcome = bridge_argument_close(range, &mut |_| {
            polls += 1;
            if polls == CANCEL_AT_POLL {
                Err(ExtractError::Cancelled)
            } else {
                Ok(())
            }
        });
        assert_eq!(outcome, Err(ExtractError::Cancelled));
        assert_eq!(polls, CANCEL_AT_POLL);
    }

    #[test]
    fn bridge_delimiter_scans_bound_their_own_byte_work() {
        let source = format!("({})", " ".repeat(MAX_BRIDGE_SCAN_BYTES));
        let range = SymbolRange {
            source: &source,
            start: 0,
            end: source.len(),
        };
        let mut polls = 0;
        let mut work = 0;
        let outcome = bridge_argument_close(range, &mut |units| {
            polls += 1;
            work += units;
            Ok(())
        });
        assert_eq!(outcome, Ok(None));
        assert_eq!(
            polls,
            MAX_BRIDGE_SCAN_BYTES / CANCELLATION_INTERVAL_BRIDGE_BYTES + 1
        );
        assert_eq!(work, MAX_BRIDGE_SCAN_BYTES);
    }
}
