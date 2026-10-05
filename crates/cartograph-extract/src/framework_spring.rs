//! Spring configuration-key signals bound to the annotated declaration.
//!
//! `@ConditionalOnProperty(prefix = "feature.payments", name = "enabled")` makes
//! its class or method depend on the `feature.payments.enabled` property. The
//! annotation itself is already a walker `Decorates` reference, so this scanner
//! reads only that annotation's argument list and emits one configuration-key
//! reference per positional, `name`, or `value` string, owned by the declaration
//! the annotation decorates (v1 `spring-conditional-on-property-binding`).
//! Array-valued arguments are not read, matching v1, and a `prefix` whose value
//! is not a plain literal makes the annotation's keys unknown.
//!
//! It also records every `@Value(...)` argument range so `${key}` placeholders
//! inside one are owned by the decorated field rather than its class.

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId};

use crate::{
    ExtractError,
    framework::{
        AnnotationInterval, DelimiterInput, FrameworkBuilder, FrameworkReferenceInput,
        matching_delimiter, skip_ascii_whitespace,
    },
};

const CONDITIONAL_ANNOTATION: &str = "ConditionalOnProperty";
const VALUE_ANNOTATION: &str = "Value";
const MAX_ANNOTATION_BYTES: usize = 4_096;
/// Arguments read from one annotation; real annotations carry a handful.
const MAX_ANNOTATION_ARGUMENTS: usize = 32;

/// One annotation whose argument list follows its walker `Decorates` reference.
struct AnnotationArguments {
    owner: SymbolId,
    /// Byte offset just after `(`.
    start: usize,
    /// Byte offset of the matching `)`.
    end: usize,
}

/// One top-level `key = value` or positional annotation argument; `value` is
/// the content of a plain string literal, or `None` for any other expression
/// (constants, arrays, Kotlin `$` templates) whose value is not known.
#[derive(Clone, Copy)]
struct ArgumentLiteral<'source> {
    key: Option<&'source str>,
    value: Option<&'source str>,
    start: usize,
}

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if !matches!(
        builder.language(),
        SourceLanguage::Java | SourceLanguage::Kotlin
    ) {
        return Ok(());
    }
    if source.contains(VALUE_ANNOTATION) {
        let intervals = value_annotation_intervals(builder, source);
        builder.set_value_annotations(intervals);
    }
    if !source.contains(CONDITIONAL_ANNOTATION) {
        return Ok(());
    }
    let annotations = annotation_arguments(builder, source, CONDITIONAL_ANNOTATION);
    for annotation in annotations {
        builder.check_cancelled()?;
        emit_conditional_keys(builder, source, &annotation)?;
    }
    Ok(())
}

/// Every declaration decorated by the `@Value` annotation whose argument list
/// contains `offset`, so `@Value("${key}") private int ttl;` attributes `key` to
/// `ttl`, and `@Value("${key}") String a, b;` to both `a` and `b` (v1 binds
/// `@Value` keys to the annotated fields). Empty when the offset is not inside
/// a `@Value` argument list.
pub(crate) fn value_annotation_owners(
    builder: &FrameworkBuilder<'_, '_>,
    offset: usize,
) -> Vec<SymbolId> {
    let intervals = builder.value_annotations();
    // Intervals are sorted by start and at most `MAX_ANNOTATION_BYTES` long, so
    // only those starting within that distance before `offset` can contain it.
    let upper = intervals.partition_point(|interval| interval.start <= offset);
    let lower = offset.saturating_sub(MAX_ANNOTATION_BYTES);
    let mut owners = Vec::new();
    let mut innermost = None;
    for interval in intervals[..upper]
        .iter()
        .rev()
        .take_while(|interval| interval.start >= lower)
    {
        if offset >= interval.end || innermost.is_some_and(|start| interval.start < start) {
            continue;
        }
        innermost = Some(interval.start);
        if !owners.contains(&interval.owner) {
            owners.push(interval.owner.clone());
        }
    }
    owners.reverse();
    owners
}

/// The argument ranges of every walker `@Value` decorator, from the
/// comment-masked source so `@Value /* note */ ("${key}")` still pairs.
fn value_annotation_intervals(
    builder: &FrameworkBuilder<'_, '_>,
    source: &str,
) -> Vec<AnnotationInterval> {
    annotation_arguments(builder, source, VALUE_ANNOTATION)
        .into_iter()
        .map(|annotation| AnnotationInterval {
            start: annotation.start,
            end: annotation.end,
            owner: annotation.owner,
        })
        .collect()
}

fn reference_end(reference: &crate::ExtractedReference) -> Option<usize> {
    usize::try_from(reference.span.end_byte()).ok()
}

/// `(start, end)` of the parenthesised argument list starting right after a
/// decorator name ending at `name_end`.
fn argument_span(source: &str, name_end: usize) -> Option<(usize, usize)> {
    let open = skip_ascii_whitespace(source, name_end);
    let close = matching_delimiter(DelimiterInput::bounded_parentheses(
        source,
        open,
        MAX_ANNOTATION_BYTES,
    ))?;
    Some((open.saturating_add(1), close))
}

fn annotation_arguments(
    builder: &FrameworkBuilder<'_, '_>,
    source: &str,
    annotation: &str,
) -> Vec<AnnotationArguments> {
    builder
        .references()
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::Decorates
                && terminal_segment(&reference.name) == annotation
        })
        .filter_map(|reference| {
            let owner = reference.owner.clone()?;
            let (start, end) = argument_span(source, reference_end(reference)?)?;
            Some(AnnotationArguments { owner, start, end })
        })
        .collect()
}

fn terminal_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn emit_conditional_keys(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    annotation: &AnnotationArguments,
) -> Result<(), ExtractError> {
    let arguments = argument_literals(source, annotation);
    let prefix = match arguments
        .iter()
        .find(|argument| argument.key == Some("prefix"))
    {
        None => "",
        Some(ArgumentLiteral {
            value: Some(prefix),
            ..
        }) => prefix,
        // A supplied prefix whose value is unknown makes every key unknown.
        Some(_) => return Ok(()),
    };
    let mut emitted = Vec::new();
    for argument in arguments
        .iter()
        .filter(|argument| matches!(argument.key, None | Some("name" | "value")))
    {
        let Some(value) = argument.value else {
            continue;
        };
        let Some(key) = join_property_key(prefix, value) else {
            continue;
        };
        if emitted.contains(&key) {
            continue;
        }
        builder.add_reference(FrameworkReferenceInput {
            owner: Some(annotation.owner.clone()),
            name: &key,
            resolution_name: None,
            kind: ReferenceKind::References,
            start: argument.start,
            end: argument.start.saturating_add(value.len()),
        })?;
        emitted.push(key);
    }
    Ok(())
}

/// v1 `joinPropertyKey`: trailing prefix dots and leading name dots are dropped.
fn join_property_key(prefix: &str, name: &str) -> Option<String> {
    let prefix = prefix.trim().trim_end_matches('.');
    let name = name.trim().trim_start_matches('.');
    match (prefix.is_empty(), name.is_empty()) {
        (_, true) => None,
        (true, false) => Some(name.to_owned()),
        (false, false) => Some(format!("{prefix}.{name}")),
    }
}

/// Top-level string-literal arguments of one annotation, in source order.
fn argument_literals<'source>(
    source: &'source str,
    annotation: &AnnotationArguments,
) -> Vec<ArgumentLiteral<'source>> {
    let Some(arguments) = source.get(annotation.start..annotation.end) else {
        return Vec::new();
    };
    top_level_segments(arguments)
        .into_iter()
        .take(MAX_ANNOTATION_ARGUMENTS)
        .map(|(offset, segment)| argument_literal(segment, annotation.start + offset))
        .collect()
}

fn argument_literal(segment: &str, offset: usize) -> ArgumentLiteral<'_> {
    let (key, value, value_offset) = match top_level_assignment(segment) {
        Some(equals) => (
            Some(segment[..equals].trim()),
            &segment[equals + 1..],
            equals + 1,
        ),
        None => (None, segment, 0),
    };
    let leading = value.len() - value.trim_start().len();
    let content = value
        .trim()
        .strip_prefix('"')
        .and_then(|literal| literal.strip_suffix('"'))
        .filter(|content| !content.contains(['"', '\\', '$']));
    ArgumentLiteral {
        key,
        value: content,
        start: offset + value_offset + leading + 1,
    }
}

/// Comma-separated segments outside quotes, parentheses, braces, and brackets.
fn top_level_segments(value: &str) -> Vec<(usize, &str)> {
    let mut segments = Vec::new();
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        if crate::framework::consume_quoted_byte(byte, &mut quote, &mut escaped) {
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'(' | b'{' | b'[' => depth = depth.saturating_add(1),
            b')' | b'}' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                segments.push((start, &value[start..index]));
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push((start, &value[start..]));
    segments
}

/// Offset of a `=` separating a named argument, outside any string literal.
fn top_level_assignment(segment: &str) -> Option<usize> {
    let quote = segment.find('"').unwrap_or(segment.len());
    segment[..quote].find('=')
}
