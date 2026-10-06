use std::{borrow::Cow, collections::BTreeSet};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};

use crate::{
    ExtractError,
    framework::{FrameworkBuilder, FrameworkReferenceInput, LandmarkInput, skip_ascii_whitespace},
    source_lines::physical_lines,
};

const MAX_SIGNAL_BYTES: usize = 4_096;
const MAX_TAGS_PER_FILE: usize = 4_096;
/// Service keys whose `@service:method` value names a method after the colon.
const METHOD_SEPARATED_SERVICE_KEYS: [&str; 2] = ["factory", "configurator"];

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    masked_source: &str,
) -> Result<(), ExtractError> {
    match builder.language() {
        SourceLanguage::Yaml if is_services_path(builder.path()) => {
            scan_services(builder, masked_source)
        }
        SourceLanguage::Php => {
            scan_hook_contracts(builder)?;
            scan_plugins(builder, masked_source)
        }
        _ => Ok(()),
    }
}

fn is_services_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".services.yml") || lower.ends_with(".services.yaml")
}

struct ServiceState {
    id: String,
    symbol_id: SymbolId,
    indent: usize,
}

#[derive(Clone, Copy)]
struct ServiceLine<'a> {
    service: &'a ServiceState,
    start: usize,
    text: &'a str,
    /// Whether the line is one of the service's own block-mapping keys
    /// (`factory:`, `class:`), not a nested argument or sequence item.
    service_setting: bool,
}

#[derive(Clone, Copy)]
struct ServiceReference<'a> {
    service: &'a ServiceState,
    value: &'a str,
    start: usize,
    end: usize,
}

fn scan_services(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    let mut state = ServiceScanState::default();
    for (line_start, line) in physical_lines(source) {
        builder.check_cancelled()?;
        if !scan_service_line(
            builder,
            &mut state,
            ServiceSourceLine::new(line_start, line),
        )? {
            break;
        }
    }
    publish_service_tags(builder, state.tag_facts)
}

type ServiceTagFact = (String, String, bool, usize, usize);

#[derive(Default)]
struct ServiceScanState {
    in_services: bool,
    services_indent: usize,
    service_indent: Option<usize>,
    current: Option<ServiceState>,
    tag_section_indent: Option<usize>,
    /// Indentation of the current service's own keys, once its first key is seen.
    member_indent: Option<usize>,
    tag_facts: BTreeSet<ServiceTagFact>,
}

#[derive(Clone, Copy)]
struct ServiceSourceLine<'source> {
    start: usize,
    text: &'source str,
    indent: usize,
}

impl<'source> ServiceSourceLine<'source> {
    fn new(start: usize, text: &'source str) -> Self {
        Self {
            start,
            text,
            indent: text.len().saturating_sub(text.trim_start().len()),
        }
    }
}

fn scan_service_line(
    builder: &mut FrameworkBuilder<'_, '_>,
    state: &mut ServiceScanState,
    line: ServiceSourceLine<'_>,
) -> Result<bool, ExtractError> {
    let trimmed = line.text.trim();
    if trimmed.is_empty() {
        return Ok(true);
    }
    if !state.in_services {
        if trimmed == "services:" {
            state.in_services = true;
            state.services_indent = line.indent;
        }
        return Ok(true);
    }
    if line.indent <= state.services_indent && !trimmed.starts_with(['-', '{', '[']) {
        return Ok(false);
    }
    if is_service_declaration(state, line.text, line.indent) {
        begin_service(builder, state, line)?;
        return Ok(true);
    }
    let Some(service) = state.current.as_ref() else {
        return Ok(true);
    };
    if line.indent <= service.indent {
        state.current = None;
        state.tag_section_indent = None;
        return Ok(true);
    }
    let mapping_key = yaml_mapping_key(line.text).filter(|_| !trimmed.starts_with('#'));
    if let Some((key, _, _)) = mapping_key {
        state.tag_section_indent = (key == "tags").then_some(line.indent);
    }
    let service_setting =
        mapping_key.is_some() && *state.member_indent.get_or_insert(line.indent) == line.indent;
    scan_service_direct_references(
        builder,
        ServiceLine {
            service,
            start: line.start,
            text: line.text,
            service_setting,
        },
    )?;
    collect_service_tags(state, line);
    Ok(true)
}

fn is_service_declaration(state: &ServiceScanState, line: &str, indent: usize) -> bool {
    yaml_mapping_key(line).is_some()
        && indent > state.services_indent
        && state
            .service_indent
            .is_none_or(|expected| indent == expected)
}

fn begin_service(
    builder: &mut FrameworkBuilder<'_, '_>,
    state: &mut ServiceScanState,
    line: ServiceSourceLine<'_>,
) -> Result<(), ExtractError> {
    let Some((id, key_start, key_end)) = yaml_mapping_key(line.text) else {
        return Ok(());
    };
    state.service_indent.get_or_insert(line.indent);
    state.tag_section_indent = None;
    state.member_indent = None;
    if id.starts_with('_') {
        state.current = None;
        return Ok(());
    }
    let symbol_id = builder.add_landmark_with_id(LandmarkInput {
        kind: SymbolKind::Resource,
        name: id.to_owned(),
        identity: format!("drupal-service::{id}"),
        start: line.start + key_start,
        end: line.start + key_end,
        body_search_text: format!("drupal service {id}"),
        target: None,
    })?;
    state.current = symbol_id.map(|symbol_id| ServiceState {
        id: id.to_owned(),
        symbol_id,
        indent: line.indent,
    });
    Ok(())
}

fn collect_service_tags(state: &mut ServiceScanState, line: ServiceSourceLine<'_>) {
    let Some(service) = state.current.as_ref() else {
        return;
    };
    if state
        .tag_section_indent
        .is_some_and(|section_indent| line.indent > section_indent)
        && let Some(tag) = yaml_value_for_key(line.text, "name")
    {
        state.tag_facts.insert((
            service.id.clone(),
            tag.value.into_owned(),
            true,
            line.start + tag.start,
            line.start + tag.end,
        ));
    }
    if let Some(tag) = tagged_iterator(line.text) {
        state.tag_facts.insert((
            service.id.clone(),
            tag.value.into_owned(),
            false,
            line.start + tag.start,
            line.start + tag.end,
        ));
    }
}

fn publish_service_tags(
    builder: &mut FrameworkBuilder<'_, '_>,
    tag_facts: BTreeSet<ServiceTagFact>,
) -> Result<(), ExtractError> {
    if tag_facts.len() > MAX_TAGS_PER_FILE {
        return Err(ExtractError::OutputLimit);
    }
    for (service_id, tag, provider, start, end) in tag_facts {
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Resource,
            name: format!("drupal-tag:{tag}"),
            identity: format!(
                "drupal-tag-{}::{tag}::{service_id}",
                if provider { "provider" } else { "consumer" }
            ),
            start,
            end,
            body_search_text: format!(
                "drupal service tag {} {tag} {service_id}",
                if provider { "provides" } else { "consumes" }
            ),
            target: None,
        })?;
    }
    Ok(())
}

fn scan_service_direct_references(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ServiceLine<'_>,
) -> Result<(), ExtractError> {
    let ServiceLine {
        service,
        start: line_start,
        text: line,
        service_setting,
    } = input;
    scan_service_scalar_references(builder, input)?;
    // A `factory`/`configurator` whose scalar is rejected (an unterminated
    // quote, an unsupported escape) names nothing rather than a raw guess.
    if yaml_first_key(line).is_some_and(|(key, ..)| {
        METHOD_SEPARATED_SERVICE_KEYS.contains(&key) && yaml_value_for_key(line, key).is_none()
    }) {
        return Ok(());
    }
    // The service's own scalar `factory` or `configurator` setting
    // `@service:method` names the service before the colon, as Symfony's
    // loader splits it; anywhere else (a `[@service, method]` sequence, a
    // nested argument mapping) the colon is part of the service id.
    let method_separated_at = METHOD_SEPARATED_SERVICE_KEYS
        .iter()
        .filter(|_| service_setting)
        .find_map(|key| {
            yaml_value_for_key(line, key)
                .filter(|scalar| scalar.value.starts_with('@'))
                .map(|scalar| scalar.start)
        });
    let mut cursor = 0_usize;
    while let Some(relative) = line[cursor..].find('@') {
        let marker = cursor + relative;
        if line.as_bytes().get(marker + 1) == Some(&b'@') {
            cursor = marker + 2;
            continue;
        }
        let name_start = marker
            + if line.as_bytes().get(marker + 1) == Some(&b'?') {
                2
            } else {
                1
            };
        let mut end = service_identifier_end(line, name_start);
        if !ends_service_identifier(line, end) {
            // The id continues with a character no service id spells here;
            // a prefix of it would name a different service.
            cursor = end.max(marker + 1);
            continue;
        }
        if method_separated_at == Some(marker)
            && let Some(colon) = line[name_start..end].find(':')
        {
            end = name_start + colon;
        }
        if end > name_start {
            add_service_reference(
                builder,
                ServiceReference {
                    service,
                    value: &line[name_start..end],
                    start: line_start + name_start,
                    end: line_start + end,
                },
            )?;
        }
        cursor = end.max(marker + 1);
    }
    Ok(())
}

fn scan_service_scalar_references(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ServiceLine<'_>,
) -> Result<(), ExtractError> {
    let ServiceLine {
        service,
        start: line_start,
        text: line,
        ..
    } = input;
    for key in ["class", "alias", "parent"] {
        if let Some(scalar) = yaml_value_for_key(line, key) {
            add_service_reference(
                builder,
                ServiceReference {
                    service,
                    value: &scalar.value,
                    start: line_start + scalar.start,
                    end: line_start + scalar.end,
                },
            )?;
        }
    }
    // A `Class::method` factory names its class; an `@service` factory is
    // one of the service references scanned by the caller.
    if let Some(factory) = yaml_value_for_key(line, "factory")
        && !factory.value.starts_with('@')
        && let Some((class, _)) = factory.value.split_once("::")
        && let Some(raw_class_length) = line[factory.start..factory.end].find("::")
    {
        // No supported escape spells `::`, so the raw and decoded texts
        // split at the same separator; the span covers the raw class text.
        add_service_reference(
            builder,
            ServiceReference {
                service,
                value: class,
                start: line_start + factory.start,
                end: line_start + factory.start + raw_class_length,
            },
        )?;
    }
    Ok(())
}

fn add_service_reference(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ServiceReference<'_>,
) -> Result<(), ExtractError> {
    let ServiceReference {
        service,
        value,
        start,
        end,
    } = input;
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_SIGNAL_BYTES || start >= end {
        return Ok(());
    }
    builder.add_reference_with_resolution(FrameworkReferenceInput {
        owner: Some(service.symbol_id.clone()),
        name: value,
        resolution_name: None,
        kind: ReferenceKind::References,
        start,
        end,
    })
}

fn scan_hook_contracts(builder: &mut FrameworkBuilder<'_, '_>) -> Result<(), ExtractError> {
    let lower_path = builder.path().to_ascii_lowercase();
    if ![".module", ".install", ".theme", ".inc"]
        .iter()
        .any(|extension| lower_path.ends_with(extension))
    {
        return Ok(());
    }
    let module = builder
        .path()
        .rsplit('/')
        .next()
        .and_then(|base| base.split('.').next())
        .unwrap_or_default()
        .replace('-', "_");
    let source = builder.source();
    for index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
        let Some((name, start, end)) = builder.original_symbol(index).and_then(|symbol| {
            matches!(symbol.kind, SymbolKind::Function | SymbolKind::Module).then(|| {
                Some((
                    symbol.name.clone(),
                    usize::try_from(symbol.span.start_byte()).ok()?,
                    usize::try_from(symbol.span.end_byte()).ok()?,
                ))
            })?
        }) else {
            continue;
        };
        let prefix_start = start.saturating_sub(MAX_SIGNAL_BYTES);
        let prefix = &source[prefix_start..start];
        let documented = documented_hook(prefix);
        let inferred = name
            .strip_prefix(&module)
            .and_then(|suffix| suffix.strip_prefix('_'))
            .filter(|suffix| !suffix.is_empty())
            .map(|suffix| format!("hook_{suffix}"));
        let Some(contract) = documented.or(inferred) else {
            continue;
        };
        let (name_start, name_end) = source[start..end]
            .find(&name)
            .map_or((start, end), |offset| {
                (start + offset, start + offset + name.len())
            });
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Resource,
            name: contract.clone(),
            identity: format!("drupal-hook-contract::{contract}::{name}"),
            start: name_start,
            end: name_end,
            body_search_text: format!("drupal hook contract {contract} implementation {name}"),
            target: Some((&name, None, name_start, name_end)),
        })?;
    }
    Ok(())
}

fn documented_hook(prefix: &str) -> Option<String> {
    let doc_end = prefix.rfind("*/")?;
    if !prefix[doc_end + 2..].trim().is_empty() {
        return None;
    }
    let doc_start = prefix[..doc_end].rfind("/**")?;
    let prefix = &prefix[doc_start..doc_end + 2];
    let marker = "@implements";
    let start = prefix.rfind(marker)? + marker.len();
    let suffix = prefix[start..].trim_start();
    let hook_start = suffix.find("hook_")?;
    let hook = &suffix[hook_start..service_identifier_end(suffix, hook_start)];
    (!hook.is_empty()).then(|| hook.to_owned())
}

fn scan_plugins(
    builder: &mut FrameworkBuilder<'_, '_>,
    masked_source: &str,
) -> Result<(), ExtractError> {
    let raw_source = builder.source();
    for index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
        let Some((class_name, start, end)) = builder.original_symbol(index).and_then(|symbol| {
            matches!(symbol.kind, SymbolKind::Class | SymbolKind::Module).then(|| {
                Some((
                    symbol.name.clone(),
                    usize::try_from(symbol.span.start_byte()).ok()?,
                    usize::try_from(symbol.span.end_byte()).ok()?,
                ))
            })?
        }) else {
            continue;
        };
        let class_marker = format!("class {class_name}");
        let Some(class_relative) = raw_source[start..end].find(&class_marker) else {
            continue;
        };
        let class_name_start = start + class_relative + "class ".len();
        let class_span = (class_name_start, class_name_start + class_name.len());
        let window_start = class_name_start.saturating_sub(MAX_SIGNAL_BYTES);
        let window_end = class_span.1;
        let raw_window = &raw_source[window_start..window_end];
        let masked_window = &masked_source[window_start..window_end];
        let annotation = plugin_annotation(raw_window);
        let attribute = plugin_attribute(masked_window);
        let Some((plugin_type, id, relative_start, relative_end)) = annotation
            .into_iter()
            .chain(attribute)
            .max_by_key(|(_, _, start, _)| *start)
        else {
            continue;
        };
        builder.add_landmark(LandmarkInput {
            kind: SymbolKind::Resource,
            name: id.to_owned(),
            identity: format!("drupal-plugin::{plugin_type}::{id}"),
            start: window_start + relative_start,
            end: window_start + relative_end,
            body_search_text: format!("drupal plugin {plugin_type} {id} {class_name}"),
            target: Some((&class_name, None, class_span.0, class_span.1)),
        })?;
    }
    Ok(())
}

fn plugin_annotation(value: &str) -> Option<(&str, &str, usize, usize)> {
    plugin_signal(value, '@', '=')
}

fn plugin_attribute(value: &str) -> Option<(&str, &str, usize, usize)> {
    plugin_signal(value, '#', ':')
}

fn plugin_signal(value: &str, prefix: char, separator: char) -> Option<(&str, &str, usize, usize)> {
    for plugin_type in [
        "Block",
        "FieldType",
        "FieldWidget",
        "FieldFormatter",
        "ViewsField",
        "QueueWorker",
        "Action",
        "Condition",
        "MigrateProcessPlugin",
    ] {
        let marker = if prefix == '#' {
            format!("#[{plugin_type}")
        } else {
            format!("{prefix}{plugin_type}")
        };
        let Some(signal) = value.rfind(&marker) else {
            continue;
        };
        let body = &value[signal + marker.len()..];
        let id_key = body.find("id")?;
        let after_id = &body[id_key + 2..];
        let separator_offset = after_id.find(separator)? + id_key + 2;
        let quoted = quoted_after(body, separator_offset + 1)?;
        return Some((
            plugin_type,
            quoted.value,
            signal + marker.len() + quoted.start,
            signal + marker.len() + quoted.end,
        ));
    }
    None
}

fn yaml_mapping_key(line: &str) -> Option<(&str, usize, usize)> {
    let content_start = line.len().saturating_sub(line.trim_start().len());
    let content = &line[content_start..];
    if content.starts_with('-') {
        return None;
    }
    let colon = content.find(':')?;
    let raw = content[..colon].trim();
    if raw.is_empty() || raw.contains(['{', '[', ',']) {
        return None;
    }
    let value = unquote(raw);
    let relative = content.find(value)?;
    Some((
        value,
        content_start + relative,
        content_start + relative + value.len(),
    ))
}

/// One YAML scalar on a line: its decoded value and the byte bounds of its
/// raw text (inside the quotes for a quoted scalar).
struct YamlScalar<'line> {
    /// Decoded value; borrowed unless a quote escape had to be decoded.
    value: Cow<'line, str>,
    /// First byte of the raw text.
    start: usize,
    /// Byte after the raw text.
    end: usize,
}

/// The scalar value of `expected` when it is the line's first key, in a
/// block mapping (`key: value`), a sequence item (`- key: value`), or a flow
/// mapping (`- { key: value, other: 1 }`).
fn yaml_value_for_key<'line>(line: &'line str, expected: &str) -> Option<YamlScalar<'line>> {
    let (key, colon, flow) = yaml_first_key(line)?;
    if key != expected {
        return None;
    }
    yaml_scalar_at(line, skip_ascii_whitespace(line, colon + 1), flow)
}

/// The line's first mapping key (trimmed), the byte offset of its `:`, and
/// whether it sits in a flow mapping.
fn yaml_first_key(line: &str) -> Option<(&str, usize, bool)> {
    let item = line.trim_start().trim_start_matches('-').trim_start();
    let flow = item.starts_with('{');
    let key_start = line.len() - item.trim_start_matches('{').trim_start().len();
    let colon = key_start + line[key_start..].find(':')?;
    Some((line[key_start..colon].trim(), colon, flow))
}

/// The scalar starting at `start`: a quoted scalar's decoded contents, a
/// flow-mapping scalar up to its `,` or `}`, or the rest of a block line
/// without trailing flow punctuation. A quoted scalar that does not close on
/// this line or uses an escape other than a quote or backslash yields nothing,
/// so no shorter or differently spelled literal is ever reported.
fn yaml_scalar_at(line: &str, start: usize, flow: bool) -> Option<YamlScalar<'_>> {
    let rest = &line[start..];
    match rest.as_bytes().first() {
        Some(b'\'') => return single_quoted(line, start + 1),
        Some(b'"') => return double_quoted(line, start + 1),
        _ => {}
    }
    let raw_end = if flow {
        rest.find([',', '}']).unwrap_or(rest.len())
    } else {
        rest.len()
    };
    let value = rest[..raw_end]
        .trim_end()
        .trim_end_matches([',', ']', '}'])
        .trim_end();
    (!value.is_empty()).then(|| YamlScalar {
        value: Cow::Borrowed(value),
        start,
        end: start + value.len(),
    })
}

/// A single-quoted scalar whose contents start at `start`; `''` is a quote.
fn single_quoted(line: &str, start: usize) -> Option<YamlScalar<'_>> {
    let bytes = line.as_bytes();
    let mut cursor = start;
    loop {
        let quote = cursor + line[cursor..].find('\'')?;
        if bytes.get(quote + 1) != Some(&b'\'') {
            let raw = &line[start..quote];
            let value = if raw.contains("''") {
                Cow::Owned(raw.replace("''", "'"))
            } else {
                Cow::Borrowed(raw)
            };
            return (!value.is_empty()).then_some(YamlScalar {
                value,
                start,
                end: quote,
            });
        }
        cursor = quote + 2;
    }
}

/// A double-quoted scalar whose contents start at `start`, decoding only the
/// `\\`, `\"` and `\/` escapes.
fn double_quoted(line: &str, start: usize) -> Option<YamlScalar<'_>> {
    let mut decoded = String::new();
    let mut escaped = false;
    let mut segment = start;
    let mut characters = line[start..].char_indices();
    while let Some((offset, character)) = characters.next() {
        let position = start + offset;
        match character {
            '"' => {
                let raw = &line[start..position];
                let value = if escaped {
                    decoded.push_str(&line[segment..position]);
                    Cow::Owned(decoded)
                } else {
                    Cow::Borrowed(raw)
                };
                return (!value.is_empty()).then_some(YamlScalar {
                    value,
                    start,
                    end: position,
                });
            }
            '\\' => {
                let (_, next) = characters.next()?;
                if !matches!(next, '\\' | '"' | '/') {
                    return None;
                }
                decoded.push_str(&line[segment..position]);
                decoded.push(next);
                segment = position + 1 + next.len_utf8();
                escaped = true;
            }
            _ => {}
        }
    }
    None
}

fn tagged_iterator(line: &str) -> Option<YamlScalar<'_>> {
    let marker = line.find("!tagged")?;
    let suffix = &line[marker..];
    let after_kind = suffix.find(char::is_whitespace)?;
    let value_start = marker + after_kind;
    let start = skip_ascii_whitespace(line, value_start);
    if line.as_bytes().get(start) == Some(&b'{') {
        return yaml_value_for_key(line, "tag");
    }
    let end = service_identifier_end(line, start);
    (end > start).then(|| YamlScalar {
        value: Cow::Borrowed(&line[start..end]),
        start,
        end,
    })
}

fn unquote(value: &str) -> &str {
    value
        .strip_circumfix('\'', '\'')
        .or_else(|| value.strip_circumfix('"', '"'))
        .unwrap_or(value)
}

/// The byte after the service-id characters starting at `cursor`: Unicode
/// alphanumerics plus `_ . - \\ : /`.
fn service_identifier_end(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .char_indices()
        .find(|&(_, character)| {
            !(character.is_alphanumeric()
                || matches!(character, '_' | '.' | '-' | '\\' | ':' | '/'))
        })
        .map_or(value.len(), |(offset, _)| cursor + offset)
}

/// Whether a service id ending at `end` ends there: at the end of the line,
/// whitespace, a quote, or flow punctuation.
fn ends_service_identifier(line: &str, end: usize) -> bool {
    line.as_bytes().get(end).is_none_or(|byte| {
        byte.is_ascii_whitespace() || matches!(*byte, b'\'' | b'"' | b',' | b']' | b'}')
    })
}

struct Quoted<'source> {
    value: &'source str,
    start: usize,
    end: usize,
}

fn quoted_after(value: &str, from: usize) -> Option<Quoted<'_>> {
    let mut cursor = from;
    while cursor < value.len() && !matches!(value.as_bytes()[cursor], b'\'' | b'"') {
        cursor += 1;
    }
    let quote = *value.as_bytes().get(cursor)?;
    let start = cursor + 1;
    cursor = start;
    while cursor < value.len() {
        if value.as_bytes()[cursor] == quote {
            return Some(Quoted {
                value: &value[start..cursor],
                start,
                end: cursor,
            });
        }
        cursor += 1;
    }
    None
}
