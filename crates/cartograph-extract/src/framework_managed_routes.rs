mod details;

use cartograph_domain::{SourceLanguage, SymbolId, SymbolKind};

use crate::{
    ExtractError,
    code_scan::CodeScan,
    framework::{
        DelimiterInput, FrameworkBuilder, FrameworkRouteInput, join_route_paths,
        matching_delimiter, quoted_literal_after, skip_ascii_whitespace,
    },
};

const MAX_ANNOTATION_BYTES: usize = 4_096;
const MAX_ROUTE_BYTES: usize = 1_024;

pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    match builder.language() {
        SourceLanguage::Java | SourceLanguage::Kotlin | SourceLanguage::Scala
            if source.contains("Mapping") =>
        {
            scan_spring(builder, source)
        }
        SourceLanguage::CSharp
            if source.contains("[Route")
                || source.contains("[Http")
                || source.contains("MapGet") =>
        {
            scan_aspnet(builder, source)
        }
        _ => Ok(()),
    }
}

fn scan_spring(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    let members = details::Members::build(builder)?;
    for class_index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
        let Some(class) = original_declaration(builder, class_index, SymbolKind::Class) else {
            continue;
        };
        let class_context = declaration_context(source, class.start, class.end);
        let class_mapping = annotation_argument(class_context, "@RequestMapping");
        let base = class_mapping
            .as_ref()
            .map_or("", |argument| argument.value_or_empty());
        for &method_index in members.of(&class.id) {
            builder.bridge.charge_work(1)?;
            let Some(method) = original_callable(builder, method_index) else {
                continue;
            };
            if !is_routable_member(source, &class, &method) {
                continue;
            }
            let method_context = declaration_context(source, method.start, method.end);
            let Some(mapping) = spring_mapping(method_context) else {
                continue;
            };
            let Some(path) = join_route_paths(base, mapping.argument.value_or_empty()) else {
                continue;
            };
            let (name_start, name_end) = symbol_name_span(source, &method);
            let (start, end) = mapping
                .argument
                .span()
                .or_else(|| class_mapping.as_ref().and_then(AnnotationArgument::span))
                .unwrap_or((name_start, name_end));
            builder.add_route(FrameworkRouteInput {
                method: mapping.method,
                path: &path,
                start,
                end,
                command: false,
                handler: Some((&method.name, name_start, name_end)),
            })?;
        }
    }
    details::landmarks(builder, source, true)
}

fn scan_aspnet(builder: &mut FrameworkBuilder<'_, '_>, source: &str) -> Result<(), ExtractError> {
    let members = details::Members::build(builder)?;
    for class_index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
        let Some(class) = original_declaration(builder, class_index, SymbolKind::Class) else {
            continue;
        };
        scan_aspnet_class(builder, source, (&class, members.of(&class.id)))?;
    }
    details::landmarks(builder, source, false)
}

fn scan_aspnet_class(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
    input: (&OriginalDeclaration, &[usize]),
) -> Result<(), ExtractError> {
    let (class, members) = input;
    let class_context = declaration_context(source, class.start, class.end);
    let class_route =
        annotation_argument(class_context, "[Route").unwrap_or(AnnotationArgument::Empty {
            start: class.start,
            end: class.start,
        });
    if matches!(class_route, AnnotationArgument::Dynamic) {
        return Ok(());
    }
    let controller = class.name.strip_suffix("Controller").unwrap_or(&class.name);
    let Some(base) = replace_route_token(class_route.value_or_empty(), "controller", controller)
    else {
        return Ok(());
    };
    for &method_index in members {
        builder.bridge.charge_work(1)?;
        let Some(method) = original_callable(builder, method_index) else {
            continue;
        };
        if !is_routable_member(source, class, &method) {
            continue;
        }
        add_aspnet_method_route(
            builder,
            AspNetMethodRoute {
                source,
                base: &base,
                class_route: &class_route,
                method: &method,
            },
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct AspNetMethodRoute<'a> {
    source: &'a str,
    base: &'a str,
    class_route: &'a AnnotationArgument<'a>,
    method: &'a OriginalDeclaration,
}

fn add_aspnet_method_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: AspNetMethodRoute<'_>,
) -> Result<(), ExtractError> {
    let AspNetMethodRoute { source, method, .. } = input;
    let method_context = declaration_context(source, method.start, method.end);
    for http in details::aspnet_mappings(method_context) {
        add_aspnet_mapping_route(builder, (input, &http))?;
    }
    Ok(())
}

fn add_aspnet_mapping_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: (AspNetMethodRoute<'_>, &Mapping<'_>),
) -> Result<(), ExtractError> {
    let (
        AspNetMethodRoute {
            source,
            base,
            class_route,
            method,
        },
        http,
    ) = input;
    let method_context = declaration_context(source, method.start, method.end);
    let route_override = annotation_argument(method_context, "[Route");
    let selected = match route_override.as_ref() {
        Some(AnnotationArgument::Dynamic) => return Ok(()),
        Some(argument) => argument,
        None => &http.argument,
    };
    if matches!(selected, AnnotationArgument::Dynamic) {
        return Ok(());
    }
    let Some(subpath) = replace_route_token(selected.value_or_empty(), "action", &method.name)
    else {
        return Ok(());
    };
    let (effective_base, effective_subpath) = effective_aspnet_paths(base, &subpath);
    let Some(path) = join_route_paths(effective_base, effective_subpath) else {
        return Ok(());
    };
    let (name_start, name_end) = symbol_name_span(source, method);
    let (start, end) = selected
        .span()
        .or_else(|| class_route.span())
        .unwrap_or((name_start, name_end));
    builder.add_route(FrameworkRouteInput {
        method: http.method,
        path: &path,
        start,
        end,
        command: false,
        handler: Some((&method.name, name_start, name_end)),
    })
}

/// Whether `method` is a member of `class` that may carry a route mapping.
///
/// A primary constructor (Kotlin, C# 12) is declared in the class header,
/// between the class annotations and the class body, so its declaration context
/// would read the class-level mapping as its own; it never handles requests.
/// Every callable declared inside the body stays routable.
fn is_routable_member(
    source: &str,
    class: &OriginalDeclaration,
    method: &OriginalDeclaration,
) -> bool {
    method.start >= class.start
        && method.end <= class.end
        && !(method.name == class.name && in_class_header(source, class.start, method.start))
}

/// Whether `offset` precedes the body of the class declared at `class_start`:
/// no `{` outside literals, comments and annotation parentheses lies between
/// them. A header longer than the annotation bound is treated as a body member.
fn in_class_header(source: &str, class_start: usize, offset: usize) -> bool {
    let Some(header) = source.as_bytes().get(class_start..offset) else {
        return false;
    };
    if header.len() > MAX_ANNOTATION_BYTES {
        return false;
    }
    let mut depth = 0_usize;
    for (_, byte) in CodeScan::new(header) {
        match byte {
            b'(' => depth = depth.saturating_add(1),
            b')' => depth = depth.saturating_sub(1),
            b'{' if depth == 0 => return false,
            _ => {}
        }
    }
    true
}

fn effective_aspnet_paths<'a>(base: &'a str, subpath: &'a str) -> (&'a str, &'a str) {
    if let Some(absolute) = subpath.strip_prefix("~/") {
        ("", absolute)
    } else if subpath.starts_with('/') {
        ("", subpath)
    } else {
        (base, subpath)
    }
}

struct Mapping<'source> {
    method: &'static str,
    argument: AnnotationArgument<'source>,
}

fn spring_mapping(context: DeclarationContext<'_>) -> Option<Mapping<'_>> {
    for (annotation, method) in [
        ("@GetMapping", "GET"),
        ("@PostMapping", "POST"),
        ("@PutMapping", "PUT"),
        ("@PatchMapping", "PATCH"),
        ("@DeleteMapping", "DELETE"),
    ] {
        if let Some(argument) = annotation_argument(context, annotation) {
            return Some(Mapping { method, argument });
        }
    }
    let argument = annotation_argument(context, "@RequestMapping")?;
    let method = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
        .into_iter()
        .find(|method| {
            context
                .iter()
                .any(|slice| slice.text.contains(&format!("RequestMethod.{method}")))
        })
        .unwrap_or("ANY");
    Some(Mapping { method, argument })
}

#[derive(Clone)]
struct OriginalDeclaration {
    id: SymbolId,
    name: String,
    start: usize,
    end: usize,
}

fn original_declaration(
    builder: &FrameworkBuilder<'_, '_>,
    index: usize,
    kind: SymbolKind,
) -> Option<OriginalDeclaration> {
    let symbol = builder.original_symbol(index)?;
    if symbol.kind != kind {
        return None;
    }
    Some(OriginalDeclaration {
        id: symbol.id.clone(),
        name: symbol.name.clone(),
        start: usize::try_from(symbol.span.start_byte()).ok()?,
        end: usize::try_from(symbol.span.end_byte()).ok()?,
    })
}

fn original_callable(
    builder: &FrameworkBuilder<'_, '_>,
    index: usize,
) -> Option<OriginalDeclaration> {
    let symbol = builder.original_symbol(index)?;
    if !matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function) {
        return None;
    }
    Some(OriginalDeclaration {
        id: symbol.id.clone(),
        name: symbol.name.clone(),
        start: usize::try_from(symbol.span.start_byte()).ok()?,
        end: usize::try_from(symbol.span.end_byte()).ok()?,
    })
}

#[derive(Clone)]
enum AnnotationArgument<'source> {
    /// No path argument; the span is the annotation's name, which declares the
    /// route when the path is inherited.
    Empty {
        start: usize,
        end: usize,
    },
    Literal {
        value: &'source str,
        start: usize,
        end: usize,
    },
    Dynamic,
}

impl<'source> AnnotationArgument<'source> {
    fn value_or_empty(&self) -> &'source str {
        match self {
            Self::Literal { value, .. } => value,
            Self::Empty { .. } | Self::Dynamic => "",
        }
    }

    const fn span(&self) -> Option<(usize, usize)> {
        match self {
            Self::Literal { start, end, .. } | Self::Empty { start, end } => Some((*start, *end)),
            Self::Dynamic => None,
        }
    }
}

fn annotation_argument<'source>(
    context: DeclarationContext<'source>,
    marker: &str,
) -> Option<AnnotationArgument<'source>> {
    for slice in context {
        if let Some(argument) = annotation_argument_in_slice(slice, marker) {
            return Some(argument);
        }
    }
    None
}

fn annotation_argument_in_slice<'source>(
    slice: ContextSlice<'source>,
    marker: &str,
) -> Option<AnnotationArgument<'source>> {
    let text = slice.text;
    let mut cursor = 0_usize;
    while let Some(relative) = text[cursor..].find(marker) {
        let start = cursor + relative;
        if start > 0 && identifier_byte(text.as_bytes()[start - 1]) {
            cursor = start + marker.len();
            continue;
        }
        let after = skip_ascii_whitespace(text, start + marker.len());
        let square_bracketed = marker.starts_with('[');
        // The annotation's name, without the attribute's opening bracket.
        let name_start = slice.offset + start + usize::from(square_bracketed);
        let name = (name_start, slice.offset + start + marker.len());
        if text.as_bytes().get(after) != Some(&b'(') {
            return Some(AnnotationArgument::Empty {
                start: name.0,
                end: name.1,
            });
        }
        return parse_annotation_parentheses(
            slice,
            AnnotationParentheses {
                open: after,
                name,
                square_bracketed,
            },
        );
    }
    None
}

/// Location of one annotation's argument list inside a context slice.
#[derive(Clone, Copy)]
struct AnnotationParentheses {
    /// Slice-relative offset of the opening parenthesis.
    open: usize,
    /// Source span of the annotation's name.
    name: (usize, usize),
    /// Whether the annotation is a C# attribute (`[Name(...)]`).
    square_bracketed: bool,
}

fn parse_annotation_parentheses(
    slice: ContextSlice<'_>,
    parentheses: AnnotationParentheses,
) -> Option<AnnotationArgument<'_>> {
    let AnnotationParentheses {
        open,
        name,
        square_bracketed,
    } = parentheses;
    let close = matching_delimiter(DelimiterInput::parentheses(slice.text, open))?;
    let argument = &slice.text[open.saturating_add(1)..close];
    if argument.trim().is_empty() {
        return Some(AnnotationArgument::Empty {
            start: name.0,
            end: name.1,
        });
    }
    if let Some(quoted) = quoted_literal_after(argument, 0)
        .filter(|quoted| !square_bracketed || details::literal_operand(argument, quoted))
        .map(|quoted| quoted.with_offset(slice.offset + open + 1))
    {
        return Some(AnnotationArgument::Literal {
            value: quoted.value,
            start: quoted.start,
            end: quoted.end,
        });
    }
    (square_bracketed || close < slice.text.len()).then_some(AnnotationArgument::Dynamic)
}

#[derive(Clone, Copy)]
struct ContextSlice<'source> {
    text: &'source str,
    offset: usize,
}

type DeclarationContext<'source> = [ContextSlice<'source>; 2];

fn declaration_context(source: &str, start: usize, end: usize) -> DeclarationContext<'_> {
    let bounded_start = start.saturating_sub(MAX_ANNOTATION_BYTES);
    let before = &source[bounded_start..start];
    let boundary = before.rfind(['}', ';', '{']).map_or(0, |offset| offset + 1);
    let prefix = &before[boundary..];
    let bounded_end = end.min(start.saturating_add(MAX_ANNOTATION_BYTES));
    let inline_end = first_unquoted_brace(&source[start..bounded_end])
        .map_or(bounded_end, |offset| start + offset);
    [
        ContextSlice {
            text: prefix,
            offset: bounded_start + boundary,
        },
        ContextSlice {
            text: &source[start..inline_end],
            offset: start,
        },
    ]
}

fn first_unquoted_brace(value: &str) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
        } else if byte == b'{' {
            return Some(index);
        }
    }
    None
}

fn symbol_name_span(source: &str, symbol: &OriginalDeclaration) -> (usize, usize) {
    let bounded_end = symbol
        .end
        .min(symbol.start.saturating_add(MAX_ANNOTATION_BYTES));
    source[symbol.start..bounded_end].find(&symbol.name).map_or(
        (symbol.start, symbol.end),
        |offset| {
            (
                symbol.start + offset,
                symbol.start + offset + symbol.name.len(),
            )
        },
    )
}

fn replace_route_token(value: &str, token: &str, replacement: &str) -> Option<String> {
    if value.len().saturating_add(replacement.len()) > MAX_ROUTE_BYTES {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    let marker = format!("[{token}]");
    let Some(index) = lower.find(&marker) else {
        return Some(value.to_owned());
    };
    let mut output = String::new();
    output
        .try_reserve(value.len().saturating_add(replacement.len()))
        .ok()?;
    output.push_str(&value[..index]);
    output.push_str(replacement);
    output.push_str(&value[index + marker.len()..]);
    Some(output)
}

fn identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphanumeric()
}
