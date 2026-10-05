use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};

use crate::{
    ExtractError,
    framework::{
        DelimiterInput, FrameworkBuilder, FrameworkNearReferenceInput, FrameworkReferenceInput,
        LandmarkInput, Quoted, javascript_identifier_at as identifier_at, matching_delimiter,
        quoted_after, skip_ascii_whitespace,
    },
};

const MAX_BRIDGE_SCAN_BYTES: usize = 4_096;
const MAX_NATIVE_ALIASES: usize = 256;
/// Every React Native method-export macro name starts with this prefix.
const REACT_NATIVE_MACRO_PREFIX: &str = "RCT_";

#[derive(Clone, Copy)]
struct BridgeLandmarkInput<'name> {
    kind: SymbolKind,
    name: &'name str,
    category: &'name str,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct EventLandmarkInput<'name> {
    event: &'name str,
    category: &'name str,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct ObjcMethodMacroInput<'source> {
    source: &'source str,
    marker: &'source str,
    module: &'source str,
    remapped: bool,
}

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

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    builder.check_cancelled()?;
    match builder.language() {
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => scan_javascript(builder, source),
        SourceLanguage::ObjectiveC => scan_objc(builder, source),
        SourceLanguage::Java | SourceLanguage::Kotlin => scan_jvm(builder, source),
        SourceLanguage::Swift => scan_swift(builder, source),
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
    if ![
        "NativeEventEmitter",
        "DeviceEventEmitter",
        "NativeModules",
        "requireNativeModule",
        "react-native",
    ]
    .into_iter()
    .any(|marker| source.contains(marker))
    {
        return Ok(());
    }
    let mut cursor = 0_usize;
    while let Some(relative) = source[cursor..].find(".addListener(") {
        builder.check_cancelled()?;
        let call = cursor + relative + ".addListener(".len();
        let Some(event) = quoted_event_after(source, call) else {
            cursor = call;
            continue;
        };
        let event_id = add_event_landmark(
            builder,
            EventLandmarkInput {
                event: event.value,
                category: "react-native-event-consumer",
                start: event.start,
                end: event.end,
            },
        )?;
        if let Some(event_id) = event_id
            && let Some(comma) = source[event.end..]
                .find(',')
                .filter(|offset| *offset <= MAX_BRIDGE_SCAN_BYTES)
                .map(|offset| event.end + offset + 1)
        {
            let handler_start = skip_ascii_whitespace(source, comma);
            if let Some((handler_end, handler)) = identifier_at(source, handler_start) {
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
        while let Some(relative) = source[cursor..].find(marker) {
            builder.check_cancelled()?;
            let call = cursor + relative + marker.len();
            let Some(event) = quoted_event_after(source, call) else {
                cursor = call;
                continue;
            };
            if seen.insert((event.start, event.end)) {
                add_event_landmark(
                    builder,
                    EventLandmarkInput {
                        event: event.value,
                        category: "react-native-event-producer",
                        start: event.start,
                        end: event.end,
                    },
                )?;
            }
            cursor = event.end;
        }
    }
    Ok(())
}

fn add_event_landmark(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: EventLandmarkInput<'_>,
) -> Result<Option<cartograph_domain::SymbolId>, ExtractError> {
    if !safe_event_name(input.event) {
        return Ok(None);
    }
    builder.add_landmark_with_id(LandmarkInput {
        kind: SymbolKind::Resource,
        name: input.event.to_owned(),
        identity: format!("{}::{}", input.category, input.event),
        start: input.start,
        end: input.end,
        body_search_text: format!(
            "react native event channel {} {}",
            input.event, input.category
        ),
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
        scan_registry_alias_invocations(
            builder,
            RegistryAliasScan {
                source,
                alias: &alias,
                module: &module,
            },
        )?;
    }
    Ok(())
}

fn collect_registry_alias_bindings(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<Vec<(String, String)>, ExtractError> {
    let mut bindings = Vec::new();
    bindings
        .try_reserve_exact(MAX_NATIVE_ALIASES)
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut seen_calls = BTreeSet::new();
    for marker in [
        "TurboModuleRegistry.get",
        "TurboModuleRegistry.getEnforcing",
        "requireNativeModule",
        "requireOptionalNativeModule",
    ] {
        let mut cursor = 0_usize;
        while bindings.len() < MAX_NATIVE_ALIASES
            && let Some(relative) = source[cursor..].find(marker)
        {
            builder.check_cancelled()?;
            let call_start = cursor + relative;
            let Some(open) = source[call_start + marker.len()..]
                .find('(')
                .map(|offset| call_start + marker.len() + offset + 1)
            else {
                cursor = call_start + marker.len();
                continue;
            };
            let Some(module) = quoted_after(source, open) else {
                cursor = open;
                continue;
            };
            if seen_calls.insert(call_start)
                && let Some(alias) = assigned_identifier_before(source, call_start)
            {
                bindings.push((alias.to_owned(), module.value.to_owned()));
            }
            cursor = module.end;
        }
    }
    Ok(bindings)
}

#[derive(Clone, Copy)]
struct RegistryAliasScan<'a> {
    source: &'a str,
    alias: &'a str,
    module: &'a str,
}

fn scan_registry_alias_invocations(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RegistryAliasScan<'_>,
) -> Result<(), ExtractError> {
    let RegistryAliasScan {
        source,
        alias,
        module,
    } = input;
    let marker = format!("{alias}.");
    let mut cursor = 0_usize;
    while let Some(relative) = source[cursor..].find(&marker) {
        builder.check_cancelled()?;
        let alias_start = cursor + relative;
        if alias_start > 0 && source.as_bytes()[alias_start - 1].is_ascii_alphanumeric() {
            cursor = alias_start + marker.len();
            continue;
        }
        let method_start = alias_start + marker.len();
        let Some((method_end, method)) = identifier_at(source, method_start) else {
            cursor = method_start;
            continue;
        };
        let call = skip_ascii_whitespace(source, method_end);
        if source.as_bytes().get(call) == Some(&b'(') && !react_native_blocklisted(method) {
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
    let Some(module) = registry_module_name(source) else {
        return Ok(());
    };
    let Some((open, close)) = interface_body(source, "Spec") else {
        return Ok(());
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        builder.check_cancelled()?;
        while cursor < close && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b';') {
            cursor += 1;
        }
        let statement_start = cursor;
        let method = identifier_at(source, cursor);
        if let Some((name_end, name)) = method {
            let after_name = skip_ascii_whitespace(source, name_end);
            if bytes.get(after_name) == Some(&b'(') && !react_native_blocklisted(name) {
                add_landmark(
                    builder,
                    BridgeLandmarkInput {
                        kind: SymbolKind::Method,
                        name,
                        category: &format!("turbo-module-spec-method::{}", module.value),
                        start: cursor,
                        end: name_end,
                    },
                )?;
            }
        }
        cursor = next_interface_statement(source, statement_start, close);
    }
    Ok(())
}

fn file_suffix(path: &str) -> Option<&str> {
    path.rsplit_once('.').map(|(_, suffix)| suffix)
}

fn registry_module_name(source: &str) -> Option<Quoted<'_>> {
    let mut selected = None;
    for marker in [
        "TurboModuleRegistry.get",
        "TurboModuleRegistry.getEnforcing",
    ] {
        let Some(call_start) = source.find(marker) else {
            continue;
        };
        let Some(open) = source[call_start + marker.len()..]
            .find('(')
            .map(|offset| call_start + marker.len() + offset + 1)
        else {
            continue;
        };
        let Some(module) = quoted_after(source, open) else {
            continue;
        };
        if selected
            .as_ref()
            .is_none_or(|retained: &Quoted<'_>| module.start < retained.start)
        {
            selected = Some(module);
        }
    }
    selected
}

fn interface_body(source: &str, expected_name: &str) -> Option<(usize, usize)> {
    let mut cursor = 0_usize;
    while let Some(relative) = source[cursor..].find("interface") {
        let marker = cursor + relative + "interface".len();
        let name_start = skip_ascii_whitespace(source, marker);
        let Some((name_end, name)) = identifier_at(source, name_start) else {
            cursor = marker;
            continue;
        };
        if name != expected_name {
            cursor = name_end;
            continue;
        }
        let open = source[name_end..].find('{')? + name_end;
        return matching_delimiter(DelimiterInput::braces(source, open)).map(|close| (open, close));
    }
    None
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
    fn consume_quote(&mut self, byte: u8, backtick: bool) -> bool {
        if let Some(active_quote) = self.quote {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == active_quote {
                self.quote = None;
            }
            return true;
        }
        if matches!(byte, b'\'' | b'"') || (backtick && byte == b'`') {
            self.quote = Some(byte);
            return true;
        }
        false
    }

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

fn next_interface_statement(source: &str, start: usize, close: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut state = BridgeDelimiterState::default();
    while cursor < close {
        let byte = bytes[cursor];
        if state.consume_quote(byte, true) {
            cursor += 1;
            continue;
        }
        if state.update_depth(byte, true) {
            cursor += 1;
            continue;
        }
        if byte == b';' && state.top_level() {
            return cursor + 1;
        }
        cursor += 1;
    }
    close
}

fn scan_native_modules_calls(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let marker = "NativeModules.";
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(marker) {
        builder.check_cancelled()?;
        let module_start = cursor + relative + marker.len();
        let Some((module_end, module)) = identifier_at(source, module_start) else {
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
        let method_start = skip_ascii_whitespace(source, module_end);
        if source.as_bytes().get(method_start) == Some(&b'.')
            && let Some((method_end, method)) = identifier_at(source, method_start + 1)
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
    for marker in [
        "TurboModuleRegistry.get",
        "TurboModuleRegistry.getEnforcing",
        "requireNativeModule",
        "requireOptionalNativeModule",
    ] {
        let mut cursor = 0;
        while let Some(relative) = source[cursor..].find(marker) {
            builder.check_cancelled()?;
            let call_start = cursor + relative;
            let Some(open) = source[call_start + marker.len()..]
                .find('(')
                .map(|offset| call_start + marker.len() + offset + 1)
            else {
                cursor = call_start + marker.len();
                continue;
            };
            let Some(module) = quoted_after(source, open) else {
                cursor = call_start + marker.len();
                continue;
            };
            if !seen_calls.insert(call_start) {
                cursor = module.end;
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
                BridgeLandmarkInput {
                    kind: SymbolKind::Resource,
                    name: module.value,
                    category: "native-module-spec",
                    start: module.start,
                    end: module.end,
                },
            )?;
            cursor = module.end;
        }
    }
    Ok(())
}

fn scan_codegen_components(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    let marker = "codegenNativeComponent";
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(marker) {
        builder.check_cancelled()?;
        let marker_start = cursor + relative;
        let Some(open) = source[marker_start + marker.len()..]
            .find('(')
            .map(|offset| marker_start + marker.len() + offset + 1)
        else {
            cursor = marker_start + marker.len();
            continue;
        };
        let Some(component) = quoted_after(source, open) else {
            cursor = open;
            continue;
        };
        add_landmark(
            builder,
            BridgeLandmarkInput {
                kind: SymbolKind::Component,
                name: component.value,
                category: "fabric-component",
                start: component.start,
                end: component.end,
            },
        )?;
        cursor = component.end;
    }
    if !source.contains("codegenNativeComponent") || !source.contains("NativeProps") {
        return Ok(());
    }
    let Some(interface) = source.find("NativeProps") else {
        return Ok(());
    };
    let Some(open) = source[interface..]
        .find('{')
        .map(|offset| interface + offset)
    else {
        return Ok(());
    };
    let Some(close) = matching_delimiter(DelimiterInput::braces(source, open)) else {
        return Ok(());
    };
    for (offset, name) in declaration_names(&source[open + 1..close]) {
        add_landmark(
            builder,
            BridgeLandmarkInput {
                kind: SymbolKind::Property,
                name,
                category: "fabric-prop",
                start: open + 1 + offset,
                end: open + 1 + offset + name.len(),
            },
        )?;
    }
    Ok(())
}

fn scan_objc(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    let class = objc_class_name(source);
    let module = objc_module_name(source, class);
    if let Some((name, start, end)) = module.as_ref() {
        add_landmark(
            builder,
            BridgeLandmarkInput {
                kind: SymbolKind::Resource,
                name,
                category: "react-native-module",
                start: *start,
                end: *end,
            },
        )?;
    }
    if let Some((module_name, _, _)) = module.as_ref() {
        for marker in [
            "RCT_EXPORT_METHOD(",
            "RCT_EXPORT_BLOCKING_SYNCHRONOUS_METHOD(",
            "RCT_EXTERN_METHOD(",
            "RCT_EXTERN__BLOCKING_SYNCHRONOUS_METHOD(",
        ] {
            scan_objc_method_macro(
                builder,
                ObjcMethodMacroInput {
                    source,
                    marker,
                    module: module_name,
                    remapped: false,
                },
            )?;
        }
        for marker in [
            "RCT_REMAP_METHOD(",
            "RCT_EXTERN_REMAP_METHOD(",
            "RCT_REMAP_BLOCKING_SYNCHRONOUS_METHOD(",
        ] {
            scan_objc_method_macro(
                builder,
                ObjcMethodMacroInput {
                    source,
                    marker,
                    module: module_name,
                    remapped: true,
                },
            )?;
        }
    }
    scan_native_view_manager(builder, source, class)?;
    scan_native_event_producers(
        builder,
        source,
        &["sendEventWithName:", "sendEventWithName("],
    )?;
    scan_objc_swift_aliases(builder, source)
}

fn scan_objc_method_macro(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ObjcMethodMacroInput<'_>,
) -> Result<(), ExtractError> {
    let ObjcMethodMacroInput {
        source,
        marker,
        module,
        remapped,
    } = input;
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(marker) {
        builder.check_cancelled()?;
        let start = cursor + relative + marker.len();
        let bounded_end = start
            .saturating_add(MAX_BRIDGE_SCAN_BYTES)
            .min(source.len());
        let body = &source[start..bounded_end];
        let name = if remapped {
            first_identifier(body)
        } else {
            first_identifier(body).map(|(offset, name)| {
                let base = name.split(':').next().unwrap_or(name);
                (offset, base)
            })
        };
        let Some((offset, name)) = name else {
            cursor = start;
            continue;
        };
        if react_native_blocklisted(name) {
            cursor = start + offset + name.len();
            continue;
        }
        add_landmark(
            builder,
            BridgeLandmarkInput {
                kind: SymbolKind::Method,
                name,
                category: &format!("react-native-method::{module}"),
                start: start + offset,
                end: start + offset + name.len(),
            },
        )?;
        cursor = start + offset + name.len();
    }
    Ok(())
}

fn scan_native_view_manager(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
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
        BridgeLandmarkInput {
            kind: SymbolKind::Component,
            name: &component,
            category: "native-view-manager",
            start,
            end,
        },
    )?;
    for marker in ["RCT_EXPORT_VIEW_PROPERTY(", "RCT_REMAP_VIEW_PROPERTY("] {
        let mut cursor = 0;
        while let Some(relative) = source[cursor..].find(marker) {
            let property_start = cursor + relative + marker.len();
            let Some((property_end, property)) = identifier_at(source, property_start) else {
                cursor = property_start;
                continue;
            };
            add_landmark(
                builder,
                BridgeLandmarkInput {
                    kind: SymbolKind::Property,
                    name: property,
                    category: &format!("native-view-prop::{component}"),
                    start: property_start,
                    end: property_end,
                },
            )?;
            cursor = property_end;
        }
    }
    Ok(())
}

fn scan_jvm(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    if let Some((module, start, end)) = react_native_jvm_module(source) {
        add_landmark(
            builder,
            BridgeLandmarkInput {
                kind: SymbolKind::Resource,
                name: &module,
                category: "react-native-module",
                start,
                end,
            },
        )?;
        scan_react_methods(builder, source, &module)?;
    }
    scan_jvm_view_manager(builder, source)?;
    scan_expo_module(builder, source)?;
    if source.contains("RCTDeviceEventEmitter")
        || source.contains("DeviceEventManagerModule")
        || source.contains("expo.modules")
    {
        scan_native_event_producers(builder, source, &[".emit(", "sendEvent("])?;
    }
    Ok(())
}

fn react_native_jvm_module(source: &str) -> Option<(String, usize, usize)> {
    if !source.contains("@ReactMethod") && !source.contains("@ReactModule") {
        return None;
    }
    if let Some(annotation) = source.find("@ReactModule")
        && let Some(module) = quoted_after(source, annotation + "@ReactModule".len())
    {
        return Some((module.value.to_owned(), module.start, module.end));
    }
    if let Some(get_name) = source.find("getName")
        && let Some(module) = quoted_after(source, get_name + "getName".len())
    {
        return Some((module.value.to_owned(), module.start, module.end));
    }
    class_name_matching(source, |name| name.ends_with("Module")).map(|(start, class)| {
        let module = class.strip_suffix("Module").unwrap_or(class);
        (module.to_owned(), start, start + class.len())
    })
}

fn scan_react_methods(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    module: &str,
) -> Result<(), ExtractError> {
    let marker = "@ReactMethod";
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(marker) {
        builder.check_cancelled()?;
        let annotation = cursor + relative;
        let start = annotation + marker.len();
        let end = start
            .saturating_add(MAX_BRIDGE_SCAN_BYTES)
            .min(source.len());
        let Some((offset, method)) = method_name_after_annotation(&source[start..end]) else {
            cursor = start;
            continue;
        };
        if !react_native_blocklisted(method) {
            add_landmark(
                builder,
                BridgeLandmarkInput {
                    kind: SymbolKind::Method,
                    name: method,
                    category: &format!("react-native-method::{module}"),
                    start: start + offset,
                    end: start + offset + method.len(),
                },
            )?;
        }
        cursor = start + offset + method.len();
    }
    Ok(())
}

fn scan_jvm_view_manager(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if !(source.contains("ViewManager") || source.contains("SimpleViewManager")) {
        return Ok(());
    }
    let Some((class_start, class)) = class_name_matching(source, |name| {
        name.ends_with("ViewManager") || name.ends_with("Manager")
    }) else {
        return Ok(());
    };
    let component = derive_component_name(class);
    add_landmark(
        builder,
        BridgeLandmarkInput {
            kind: SymbolKind::Component,
            name: &component,
            category: "native-view-manager",
            start: class_start,
            end: class_start + class.len(),
        },
    )?;
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("@ReactProp") {
        let annotation = cursor + relative;
        let Some(property) = quoted_after(source, annotation + "@ReactProp".len()) else {
            cursor = annotation + "@ReactProp".len();
            continue;
        };
        add_landmark(
            builder,
            BridgeLandmarkInput {
                kind: SymbolKind::Property,
                name: property.value,
                category: &format!("native-view-prop::{component}"),
                start: property.start,
                end: property.end,
            },
        )?;
        cursor = property.end;
    }
    Ok(())
}

fn scan_swift(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    scan_expo_module(builder, source)?;
    scan_swift_objc_exports(builder, source)?;
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
        builder.check_cancelled()?;
        let Some(method) = objc_alias_source(builder, index) else {
            continue;
        };
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
                BridgeLandmarkInput {
                    kind: SymbolKind::Method,
                    name: &alias,
                    category: &format!("objc-swift-method::{}", method.selector),
                    start: method.start,
                    end: method.end,
                },
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
) -> Result<(), ExtractError> {
    for index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
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
        if symbol_has_swift_attribute(NamedSymbolRange {
            range,
            name: "nonobjc",
        }) {
            continue;
        }
        let explicit = symbol_has_swift_attribute(NamedSymbolRange {
            range,
            name: "objc",
        });
        if !explicit && !containing_objc_members_class(builder, range) {
            continue;
        }
        let (span_start, span_end) = symbol_name_span(NamedSymbolRange { range, name: &name });
        let mut aliases = BTreeSet::from([name.clone()]);
        if let Some(selector) = swift_objc_selector_attribute(source, start, end) {
            aliases.extend(swift_base_names_for_objc_selector(&selector));
        }
        for alias in aliases {
            add_landmark(
                builder,
                BridgeLandmarkInput {
                    kind: SymbolKind::Method,
                    name: &alias,
                    category: &format!("swift-objc-method::{name}"),
                    start: span_start,
                    end: span_end,
                },
            )?;
        }
    }
    Ok(())
}

fn containing_objc_members_class(
    builder: &FrameworkBuilder<'_, '_>,
    member: SymbolRange<'_>,
) -> bool {
    let mut containing: Option<(usize, usize)> = None;
    for index in 0..builder.original_symbol_count() {
        let Some(symbol) = builder.original_symbol(index) else {
            continue;
        };
        if symbol.kind != SymbolKind::Class {
            continue;
        }
        let (Ok(start), Ok(end)) = (
            usize::try_from(symbol.span.start_byte()),
            usize::try_from(symbol.span.end_byte()),
        ) else {
            continue;
        };
        if start <= member.start
            && member.end <= end
            && containing.is_none_or(|(retained_start, retained_end)| {
                end.saturating_sub(start) < retained_end.saturating_sub(retained_start)
            })
        {
            containing = Some((start, end));
        }
    }
    containing.is_some_and(|(start, end)| {
        symbol_has_swift_attribute(NamedSymbolRange {
            range: SymbolRange {
                source: member.source,
                start,
                end,
            },
            name: "objcMembers",
        })
    })
}

fn symbol_has_swift_attribute(input: NamedSymbolRange<'_, '_>) -> bool {
    swift_symbol_attribute_text(input.range.source, input.range.start, input.range.end)
        .into_iter()
        .any(|text| contains_swift_attribute(text, input.name))
}

fn swift_objc_selector_attribute(source: &str, start: usize, end: usize) -> Option<String> {
    for text in swift_symbol_attribute_text(source, start, end) {
        let marker = "@objc(";
        let Some(open) = text.find(marker).map(|offset| offset + marker.len()) else {
            continue;
        };
        let close = text[open..].find(')')? + open;
        let selector = text[open..close].trim();
        if !selector.is_empty()
            && selector.len() <= 512
            && selector
                .bytes()
                .all(|byte| byte == b':' || byte == b'_' || byte.is_ascii_alphanumeric())
        {
            return Some(selector.to_owned());
        }
    }
    None
}

fn swift_symbol_attribute_text(source: &str, start: usize, end: usize) -> [&str; 2] {
    let bounded_end = end.min(start.saturating_add(MAX_BRIDGE_SCAN_BYTES));
    let inline = source.get(start..bounded_end).unwrap_or_default();
    let line_start = source[..start.min(source.len())]
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    let mut prefix_start = line_start;
    let mut cursor = line_start;
    for _ in 0..8 {
        if cursor == 0 {
            break;
        }
        let previous_end = cursor.saturating_sub(1);
        let previous_start = source[..previous_end]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        let line = source[previous_start..previous_end].trim();
        if !line.starts_with('@') {
            break;
        }
        prefix_start = previous_start;
        cursor = previous_start;
    }
    let prefix = source.get(prefix_start..start).unwrap_or_default();
    [prefix, inline]
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

fn scan_expo_module(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if !(source.contains(": Module") || source.contains("extends Module"))
        || !(source.contains("Function(")
            || source.contains("AsyncFunction(")
            || source.contains("Property("))
    {
        return Ok(());
    }
    let class = class_name(source);
    let module = source
        .find("Name(")
        .and_then(|start| {
            quoted_after(source, start + "Name(".len())
                .map(|name| (name.value, name.start, name.end))
        })
        .or_else(|| class.map(|(start, name)| (name, start, start + name.len())));
    let Some((module, module_start, module_end)) = module else {
        return Ok(());
    };
    add_landmark(
        builder,
        BridgeLandmarkInput {
            kind: SymbolKind::Resource,
            name: module,
            category: "expo-module",
            start: module_start,
            end: module_end,
        },
    )?;
    let mut seen = BTreeSet::new();
    for marker in ["AsyncFunction(", "Function(", "Property("] {
        let mut cursor = 0;
        while let Some(relative) = source[cursor..].find(marker) {
            builder.check_cancelled()?;
            let call = cursor + relative + marker.len();
            let Some(member) = quoted_after(source, call) else {
                cursor = call;
                continue;
            };
            if !react_native_blocklisted(member.value) && seen.insert(member.value.to_owned()) {
                add_landmark(
                    builder,
                    BridgeLandmarkInput {
                        kind: SymbolKind::Method,
                        name: member.value,
                        category: &format!("expo-module-method::{module}"),
                        start: member.start,
                        end: member.end,
                    },
                )?;
            }
            cursor = member.end;
        }
    }
    Ok(())
}

fn add_landmark(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: BridgeLandmarkInput<'_>,
) -> Result<(), ExtractError> {
    add_landmark_with_id(builder, input).map(|_| ())
}

fn add_landmark_with_id(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: BridgeLandmarkInput<'_>,
) -> Result<Option<SymbolId>, ExtractError> {
    if input.name.is_empty() || input.start >= input.end || input.end > builder.source().len() {
        return Ok(None);
    }
    builder.add_landmark_with_id(LandmarkInput {
        kind: input.kind,
        name: input.name.to_owned(),
        identity: format!("{}::{}", input.category, input.name),
        start: input.start,
        end: input.end,
        body_search_text: format!("{} {}", input.category, input.name),
        target: None,
    })
}

fn objc_module_name<'source>(
    source: &'source str,
    class: Option<(&'source str, usize, usize)>,
) -> Option<(String, usize, usize)> {
    let mut saw_export_module = false;
    for marker in [
        "RCT_EXTERN_REMAP_MODULE(",
        "RCT_EXTERN_MODULE(",
        "RCT_EXPORT_MODULE(",
    ] {
        let Some(start) = source.find(marker).map(|offset| offset + marker.len()) else {
            continue;
        };
        saw_export_module = true;
        if let Some((end, name)) = identifier_at(source, skip_ascii_whitespace(source, start))
            && name != "RCT_EXPORT_MODULE"
        {
            return Some((name.to_owned(), end - name.len(), end));
        }
    }
    saw_export_module
        .then_some(class)
        .flatten()
        .map(|(name, start, end)| {
            let stripped = name
                .strip_prefix("RCT")
                .filter(|value| !value.is_empty())
                .unwrap_or(name);
            (stripped.to_owned(), start, end)
        })
}

fn objc_class_name(source: &str) -> Option<(&str, usize, usize)> {
    for marker in ["@implementation", "@interface"] {
        if let Some(start) = source.find(marker).map(|offset| offset + marker.len()) {
            let start = skip_ascii_whitespace(source, start);
            if let Some((end, name)) = identifier_at(source, start) {
                return Some((name, start, end));
            }
        }
    }
    None
}

fn class_name(source: &str) -> Option<(usize, &str)> {
    class_name_matching(source, |_| true)
}

fn class_name_matching(source: &str, predicate: impl Fn(&str) -> bool) -> Option<(usize, &str)> {
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("class ") {
        let marker = cursor + relative + "class ".len();
        let start = skip_ascii_whitespace(source, marker);
        let Some((end, name)) = identifier_at(source, start) else {
            cursor = marker;
            continue;
        };
        if predicate(name) {
            return Some((start, name));
        }
        cursor = end;
    }
    None
}

fn derive_component_name(class: &str) -> String {
    let stripped = class.strip_prefix("RCT").unwrap_or(class);
    stripped
        .strip_suffix("ViewManager")
        .or_else(|| stripped.strip_suffix("Manager"))
        .unwrap_or(stripped)
        .to_owned()
}

fn method_name_after_annotation(value: &str) -> Option<(usize, &str)> {
    if let Some(fun) = value.find("fun ") {
        let start = skip_ascii_whitespace(value, fun + "fun ".len());
        let (end, name) = identifier_at(value, start)?;
        return (end > start).then_some((start, name));
    }
    let open = value.find('(')?;
    first_identifier(&value[..open])
        .into_iter()
        .chain(all_identifiers(&value[..open]))
        .filter(|(_, name)| {
            !matches!(
                *name,
                "public"
                    | "private"
                    | "protected"
                    | "static"
                    | "final"
                    | "suspend"
                    | "void"
                    | "String"
                    | "Boolean"
                    | "Int"
                    | "Double"
            )
        })
        .last()
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

fn first_identifier(value: &str) -> Option<(usize, &str)> {
    all_identifiers(value).next()
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
