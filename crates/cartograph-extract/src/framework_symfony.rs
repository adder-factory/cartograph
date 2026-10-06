//! Symfony `#[Route]` attribute routes.
//!
//! Only attributes whose name the PHP walker resolved, with the `use` imports
//! of their own namespace block, to Symfony's routing `Route` attribute class
//! are routes; the walker records that resolution on the attribute's
//! `Decorates` reference.
//! A class-level route supplies a path prefix, route-name prefix, and HTTP
//! methods that every method-level route of the class concatenates or merges,
//! as Symfony's attribute loader does. A class whose routes are only
//! class-level and that declares `__invoke` routes to `__invoke`. Each route
//! is one landmark named by its prefixed route name, or by its path when it
//! has no name, that calls the method it decorates. Only literal paths are
//! admitted: localized path maps and computed values are skipped, and a
//! computed class prefix suppresses the whole class rather than being
//! approximated as empty.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};

use crate::{
    ExtractError, PHP_EXACT_RESOLUTION_PREFIX,
    framework::{
        DelimiterInput, FrameworkBuilder, LandmarkInput, consume_quoted_byte, matching_delimiter,
        quoted_literal_after, safe_route_value, skip_ascii_whitespace,
    },
};

/// Exact class lookups of Symfony's route attribute classes (the annotation
/// class is an alias of the attribute class).
const SYMFONY_ROUTE_CLASS_KEYS: [&str; 2] = [
    "class::Symfony\\Component\\Routing\\Attribute::Route",
    "class::Symfony\\Component\\Routing\\Annotation::Route",
];
/// The method Symfony routes a class-only route to.
const INVOKE_METHOD: &str = "__invoke";
/// Bytes scanned for attribute groups before a declaration keyword.
const MAX_ATTRIBUTE_BYTES: usize = 4_096;
/// Route attributes admitted on one declaration.
const MAX_ROUTES_PER_DECLARATION: usize = 16;
/// Arguments or attributes parsed from one comma-separated list.
const MAX_LIST_ITEMS: usize = 32;
/// HTTP methods admitted on one route.
const MAX_ROUTE_METHODS: usize = 8;
/// Longest admitted HTTP method token.
const MAX_METHOD_BYTES: usize = 16;
/// Positional argument index of the route name (`#[Route('/path', 'name')]`).
const POSITIONAL_NAME_INDEX: usize = 1;
/// Positional argument index of the HTTP methods in Symfony's route
/// constructor (path, name, requirements, options, defaults, host, methods).
const POSITIONAL_METHODS_INDEX: usize = 6;

/// Publish the Symfony attribute routes of one PHP file.
pub(crate) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    source: &str,
) -> Result<(), ExtractError> {
    if builder.language() != SourceLanguage::Php || !source.contains("#[") {
        return Ok(());
    }
    let route_names = RouteAttributeNames::new(builder)?;
    if route_names.starts.is_empty() {
        return Ok(());
    }
    let controllers = controllers(builder)?;
    for controller in &controllers {
        builder.check_cancelled()?;
        scan_controller(
            builder,
            ControllerScan {
                source,
                route_names: &route_names,
                controller,
            },
        )?;
    }
    Ok(())
}

/// The byte offsets of attribute names the PHP walker resolved to Symfony's
/// route attribute class.
struct RouteAttributeNames {
    starts: BTreeSet<usize>,
}

impl RouteAttributeNames {
    fn new(builder: &mut FrameworkBuilder<'_, '_>) -> Result<Self, ExtractError> {
        let mut starts = BTreeSet::new();
        for index in 0..builder.reference_count() {
            builder.check_cancelled()?;
            let Some(reference) = builder.reference(index) else {
                continue;
            };
            let symfony_route = reference.kind == ReferenceKind::Decorates
                && reference
                    .resolution_name
                    .as_deref()
                    .and_then(|lookup| lookup.strip_prefix(PHP_EXACT_RESOLUTION_PREFIX))
                    .is_some_and(|lookup| SYMFONY_ROUTE_CLASS_KEYS.contains(&lookup));
            if symfony_route && let Ok(start) = usize::try_from(reference.span.start_byte()) {
                starts.insert(start);
            }
        }
        Ok(Self { starts })
    }

    fn accepts(&self, start: usize) -> bool {
        self.starts.contains(&start)
    }
}

/// One original PHP declaration and its byte span.
struct Declaration {
    name: String,
    qualified_name: String,
    start: usize,
    end: usize,
}

/// A class and the methods it declares directly.
struct Controller {
    class: Declaration,
    methods: Vec<Declaration>,
}

/// Index classes by qualified name, then attach each method to the one class
/// of that name whose span contains it, in one pass over the original
/// symbols. Conditional duplicate declarations keep their own methods.
fn controllers(builder: &mut FrameworkBuilder<'_, '_>) -> Result<Vec<Controller>, ExtractError> {
    let mut controllers = Vec::new();
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
        if let Some(class) = declaration(builder, index, SymbolKind::Class) {
            by_name
                .entry(class.qualified_name.clone())
                .or_default()
                .push(controllers.len());
            controllers.push(Controller {
                class,
                methods: Vec::new(),
            });
        }
    }
    for index in 0..builder.original_symbol_count() {
        builder.check_cancelled()?;
        let Some(method) = declaration(builder, index, SymbolKind::Method) else {
            continue;
        };
        let owner = method
            .qualified_name
            .rsplit_once("::")
            .and_then(|(owner, _)| by_name.get(owner))
            .and_then(|candidates| containing_class(&controllers, candidates, &method));
        if let Some(controller) = owner.and_then(|owner| controllers.get_mut(owner)) {
            controller.methods.push(method);
        }
    }
    Ok(controllers)
}

/// The same-name class, among `candidates` in source order, whose span
/// contains `method`: the last one starting at or before the method.
fn containing_class(
    controllers: &[Controller],
    candidates: &[usize],
    method: &Declaration,
) -> Option<usize> {
    let preceding = candidates.partition_point(|candidate| {
        controllers
            .get(*candidate)
            .is_some_and(|controller| controller.class.start <= method.start)
    });
    let candidate = *candidates.get(preceding.checked_sub(1)?)?;
    let class = &controllers.get(candidate)?.class;
    (method.end <= class.end).then_some(candidate)
}

/// One original declaration of `kind`.
fn declaration(
    builder: &FrameworkBuilder<'_, '_>,
    index: usize,
    kind: SymbolKind,
) -> Option<Declaration> {
    let symbol = builder.original_symbol(index)?;
    (symbol.kind == kind).then_some(())?;
    Some(Declaration {
        name: symbol.name.clone(),
        qualified_name: symbol.qualified_name.clone(),
        start: usize::try_from(symbol.span.start_byte()).ok()?,
        end: usize::try_from(symbol.span.end_byte()).ok()?,
    })
}

/// One controller scan in one file.
#[derive(Clone, Copy)]
struct ControllerScan<'scan> {
    source: &'scan str,
    route_names: &'scan RouteAttributeNames,
    controller: &'scan Controller,
}

/// Publish the method-level routes of one controller, or its class-level
/// routes on `__invoke` when no method declares a route.
fn scan_controller(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: ControllerScan<'_>,
) -> Result<(), ExtractError> {
    let class_routes = attribute_routes(input, input.controller.class.start);
    if class_routes.dynamic {
        return Ok(());
    }
    let globals = class_routes.routes.first();
    let mut method_routes_declared = false;
    for method in &input.controller.methods {
        builder.check_cancelled()?;
        let method_routes = attribute_routes(input, method.start);
        method_routes_declared |= method_routes.dynamic || !method_routes.routes.is_empty();
        let handler = method_name_span(input.source, method, method_routes.end);
        for route in &method_routes.routes {
            add_route(
                builder,
                RouteEmission {
                    globals,
                    route,
                    method,
                    handler,
                },
            )?;
        }
    }
    if method_routes_declared || globals.is_none() {
        return Ok(());
    }
    let Some(method) = input
        .controller
        .methods
        .iter()
        .find(|method| method.name.eq_ignore_ascii_case(INVOKE_METHOD))
    else {
        return Ok(());
    };
    let attributes_end = attribute_routes(input, method.start).end;
    let handler = method_name_span(input.source, method, attributes_end);
    for route in &class_routes.routes {
        add_route(
            builder,
            RouteEmission {
                globals: None,
                route,
                method,
                handler,
            },
        )?;
    }
    Ok(())
}

/// The literal facts of one route attribute.
#[derive(Default)]
struct RouteAttribute<'source> {
    path: Option<&'source str>,
    name: Option<&'source str>,
    methods: Vec<&'source str>,
    start: usize,
    end: usize,
}

/// Route attributes in the attribute groups leading a declaration.
struct AttributeScan<'source> {
    routes: Vec<RouteAttribute<'source>>,
    /// A route attribute whose arguments are not literal.
    dynamic: bool,
    /// Offset where the leading attribute groups end.
    end: usize,
}

/// Parse the attribute groups that lead the declaration at `start`.
fn attribute_routes(input: ControllerScan<'_>, start: usize) -> AttributeScan<'_> {
    let source = input.source;
    let mut limit = source.len().min(start.saturating_add(MAX_ATTRIBUTE_BYTES));
    while !source.is_char_boundary(limit) {
        limit -= 1;
    }
    let bounded = &source[..limit];
    let mut scan = AttributeScan {
        routes: Vec::new(),
        dynamic: false,
        end: skip_ascii_whitespace(bounded, start),
    };
    while bounded[scan.end..].starts_with("#[") && scan.routes.len() < MAX_ROUTES_PER_DECLARATION {
        let open = scan.end + 1;
        let Some(close) = matching_delimiter(DelimiterInput::square_brackets(bounded, open)) else {
            break;
        };
        for (offset, attribute) in top_level_items(&bounded[open + 1..close]) {
            match route_attribute(
                bounded,
                AttributeSite {
                    offset: open + 1 + offset,
                    text: attribute,
                    route_names: input.route_names,
                },
            ) {
                AttributeParse::Route(route) => scan.routes.push(route),
                AttributeParse::Dynamic => scan.dynamic = true,
                AttributeParse::Other => {}
            }
        }
        scan.end = skip_ascii_whitespace(bounded, close + 1);
    }
    scan.routes.truncate(MAX_ROUTES_PER_DECLARATION);
    scan
}

/// Split a comma-separated list at depth zero, outside quotes.
fn top_level_items(list: &str) -> Vec<(usize, &str)> {
    let mut items = Vec::new();
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut item_start = 0;
    for (index, byte) in list.bytes().enumerate() {
        if consume_quoted_byte(byte, &mut quote, &mut escaped) {
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' | b'[' | b'{' => depth = depth.saturating_add(1),
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                items.push((item_start, &list[item_start..index]));
                item_start = index + 1;
            }
            _ => {}
        }
        if items.len() >= MAX_LIST_ITEMS {
            return items;
        }
    }
    if !list[item_start..].trim().is_empty() {
        items.push((item_start, &list[item_start..]));
    }
    items
}

/// One attribute inside an attribute group.
#[derive(Clone, Copy)]
struct AttributeSite<'site> {
    offset: usize,
    text: &'site str,
    route_names: &'site RouteAttributeNames,
}

/// How one attribute in a group parsed.
enum AttributeParse<'source> {
    Route(RouteAttribute<'source>),
    Dynamic,
    Other,
}

/// Parse one attribute; only resolved Symfony route attributes are routes.
fn route_attribute<'source>(
    source: &'source str,
    site: AttributeSite<'_>,
) -> AttributeParse<'source> {
    let leading = site.text.len() - site.text.trim_start().len();
    let start = site.offset + leading;
    let text = site.text.trim();
    let name_end = text
        .find(|character: char| {
            !(character.is_alphanumeric()
                || !character.is_ascii()
                || matches!(character, '_' | '\\'))
        })
        .unwrap_or(text.len());
    if name_end == 0 || !site.route_names.accepts(start) {
        return AttributeParse::Other;
    }
    let open = start + name_end + (text[name_end..].len() - text[name_end..].trim_start().len());
    if source.as_bytes().get(open) != Some(&b'(') {
        return AttributeParse::Route(RouteAttribute {
            start,
            end: start + name_end,
            ..RouteAttribute::default()
        });
    }
    let Some(close) = matching_delimiter(DelimiterInput::parentheses(source, open)) else {
        return AttributeParse::Dynamic;
    };
    match route_arguments(&source[open + 1..close]) {
        Some(route) => AttributeParse::Route(RouteAttribute {
            start,
            end: close + 1,
            ..route
        }),
        None => AttributeParse::Dynamic,
    }
}

/// The literal path, name, and methods of a route attribute, or `None`
/// when any of them is computed.
fn route_arguments(arguments: &str) -> Option<RouteAttribute<'_>> {
    let mut route = RouteAttribute::default();
    for (position, (_, argument)) in top_level_items(arguments).into_iter().enumerate() {
        let (label, value) = named_argument(argument);
        match (label, position) {
            (Some("path"), _) | (None, 0) => route.path = optional_string(value)?.text(),
            (Some("name"), _) | (None, POSITIONAL_NAME_INDEX) => {
                route.name = optional_string(value)?.text();
            }
            (Some("methods"), _) | (None, POSITIONAL_METHODS_INDEX) => {
                route.methods = http_methods(value)?;
            }
            _ => {}
        }
    }
    Some(route)
}

/// A nullable string argument: an explicit `null` or a plain literal.
enum OptionalString<'source> {
    Null,
    Text(&'source str),
}

impl<'source> OptionalString<'source> {
    const fn text(self) -> Option<&'source str> {
        match self {
            Self::Null => None,
            Self::Text(text) => Some(text),
        }
    }
}

/// `null` is an absent value; anything else must be a plain string literal.
fn optional_string(value: &str) -> Option<OptionalString<'_>> {
    if value.trim().eq_ignore_ascii_case("null") {
        Some(OptionalString::Null)
    } else {
        string_literal(value).map(OptionalString::Text)
    }
}

/// Split `label: value` named arguments from positional ones.
fn named_argument(argument: &str) -> (Option<&str>, &str) {
    let trimmed = argument.trim();
    let label_end = trimmed
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(trimmed.len());
    let rest = trimmed[label_end..].trim_start();
    match rest.strip_prefix(':') {
        Some(value) if label_end > 0 && !value.starts_with(':') => {
            (Some(&trimmed[..label_end]), value.trim())
        }
        _ => (None, trimmed),
    }
}

/// The value of one complete quoted literal whose source spelling is its
/// value: interpolation and escape sequences are rejected rather than
/// published undecoded.
fn string_literal(value: &str) -> Option<&str> {
    let value = value.trim();
    let quoted = quoted_literal_after(value, 0)?;
    let complete = quoted.start == 1 && quoted.end.checked_add(1) == Some(value.len());
    let interpolated = value.starts_with('"') && quoted.value.contains('$');
    (complete && !interpolated && !quoted.value.contains('\\')).then_some(quoted.value)
}

/// One literal HTTP method or a literal list of them. A list longer than
/// the method bound is rejected rather than truncated, so an entry past the
/// bound can never be silently dropped or left unchecked.
fn http_methods(value: &str) -> Option<Vec<&str>> {
    let value = value.trim();
    let entries = match value
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
    {
        Some(inner) => top_level_items(inner)
            .into_iter()
            .map(|(_, entry)| entry)
            .collect(),
        None => vec![value],
    };
    if entries.len() > MAX_ROUTE_METHODS {
        return None;
    }
    let mut methods = Vec::new();
    for entry in entries {
        let method = string_literal(entry)?;
        if method.is_empty()
            || method.len() > MAX_METHOD_BYTES
            || !method.bytes().all(|byte| byte.is_ascii_alphabetic())
        {
            return None;
        }
        methods.push(method);
    }
    Some(methods)
}

/// The byte span of a method's name: the identifier that follows the
/// `function` keyword (and an optional by-reference `&`) after the
/// declaration's attribute groups.
fn method_name_span(source: &str, method: &Declaration, from: usize) -> (usize, usize) {
    const KEYWORD: &str = "function";
    let mut limit = method.end.min(from.saturating_add(MAX_ATTRIBUTE_BYTES));
    while !source.is_char_boundary(limit) {
        limit -= 1;
    }
    source
        .get(from..limit)
        .and_then(|window| {
            let after_keyword = window.find(KEYWORD)? + KEYWORD.len();
            let name = window[after_keyword..]
                .find(|character: char| !(character.is_whitespace() || character == '&'))?
                + after_keyword;
            if !window[name..].starts_with(&method.name) {
                return None;
            }
            let end = name + method.name.len();
            let boundary = window[end..]
                .chars()
                .next()
                .is_none_or(|character| !(character.is_alphanumeric() || character == '_'));
            boundary.then_some((from + name, from + end))
        })
        .unwrap_or((method.start, method.end))
}

/// One route attribute resolved against its class globals and handler.
#[derive(Clone, Copy)]
struct RouteEmission<'scan> {
    globals: Option<&'scan RouteAttribute<'scan>>,
    route: &'scan RouteAttribute<'scan>,
    method: &'scan Declaration,
    handler: (usize, usize),
}

/// Publish one route joined with its class prefix.
fn add_route(
    builder: &mut FrameworkBuilder<'_, '_>,
    input: RouteEmission<'_>,
) -> Result<(), ExtractError> {
    let globals = input.globals;
    let prefix = globals.and_then(|globals| globals.path).unwrap_or_default();
    let method_path = input.route.path.unwrap_or_default();
    if ![prefix, method_path]
        .into_iter()
        .all(path_components_are_safe)
    {
        return Ok(());
    }
    let joined = format!("{prefix}{method_path}");
    let Some(path) = normalized_path(&joined) else {
        return Ok(());
    };
    let joined_name = match input.route.name {
        Some(name) => format!(
            "{}{name}",
            globals.and_then(|globals| globals.name).unwrap_or_default()
        ),
        None => path.clone(),
    };
    let Some(name) = safe_route_value(&joined_name, false) else {
        return Ok(());
    };
    // The loader concatenates route names without trimming either part.
    if name != joined_name {
        return Ok(());
    }
    let methods = merged_methods(globals, input.route);
    builder.add_landmark(LandmarkInput {
        kind: SymbolKind::Route,
        identity: format!("symfony-route::{methods}::{path}::{name}"),
        body_search_text: format!("symfony route {name} {methods} {path}"),
        name,
        start: input.route.start,
        end: input.route.end,
        target: Some((
            &input.method.name,
            Some(&input.method.qualified_name),
            input.handler.0,
            input.handler.1,
        )),
    })
}

/// Normalize the joined path without hiding a sensitive prefix behind `/`.
fn normalized_path(joined: &str) -> Option<String> {
    if !path_components_are_safe(joined) {
        return None;
    }
    // Route::setPath trims PHP's ASCII trim characters, then replaces every
    // leading slash with one. Screen the content before adding that slash.
    let content = joined
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\u{b}'])
        .trim_start_matches('/');
    if !content.is_empty() {
        safe_route_value(content, false)?;
    }
    let normalized = format!("/{content}");
    let path = safe_route_value(&normalized, false)?;
    // The validator trims Unicode whitespace, which PHP leaves in patterns.
    // Never publish a different endpoint if validation changed bytes.
    (path == normalized).then_some(path)
}

/// Screen the whole path and every segment, allowing empty root segments.
fn path_components_are_safe(value: &str) -> bool {
    std::iter::once(value)
        .chain(value.split('/'))
        .filter(|component| !component.trim().is_empty())
        .all(|component| safe_route_value(component, false).is_some())
}

/// Class-level methods followed by the route's own, uppercased, without
/// repeats; `ANY` when neither restricts the route.
fn merged_methods(globals: Option<&RouteAttribute<'_>>, route: &RouteAttribute<'_>) -> String {
    let mut methods: Vec<String> = Vec::new();
    for method in globals
        .map_or(&[] as &[&str], |globals| globals.methods.as_slice())
        .iter()
        .chain(&route.methods)
    {
        let method = method.to_ascii_uppercase();
        if !methods.contains(&method) {
            methods.push(method);
        }
    }
    if methods.is_empty() {
        "ANY".to_owned()
    } else {
        methods.join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_arguments_accept_positional_named_and_method_forms() {
        let Some(route) = route_arguments("'/a/{id}', name: 'show', methods: ['get', \"POST\"]")
        else {
            panic!("literal route arguments must parse");
        };
        assert_eq!(route.path, Some("/a/{id}"));
        assert_eq!(route.name, Some("show"));
        assert_eq!(route.methods, ["get", "POST"]);

        let Some(named) = route_arguments("name: 'x', path: '/p', methods: 'PUT'") else {
            panic!("named path form must parse");
        };
        assert_eq!(named.path, Some("/p"));
        assert_eq!(named.methods, ["PUT"]);
        assert_eq!(
            route_arguments("'/p', 'positional_name'").and_then(|route| route.name),
            Some("positional_name")
        );
        assert_eq!(
            route_arguments("name: 'only_name'").and_then(|route| route.path),
            None,
            "an omitted path is absent, not dynamic"
        );
        let Some(nullable) = route_arguments("'/p', null") else {
            panic!("a null name is absent, not dynamic");
        };
        assert_eq!(nullable.name, None);
        let Some(positional) = route_arguments("'/p', 'p', [], [], [], null, ['POST']") else {
            panic!("positional methods must parse");
        };
        assert_eq!(positional.methods, ["POST"]);
    }

    #[test]
    fn computed_paths_names_and_methods_are_rejected_rather_than_approximated() {
        assert!(route_arguments("['en' => '/about']").is_none());
        assert!(route_arguments("self::PATH").is_none());
        assert!(route_arguments("'/a' . $suffix").is_none());
        assert!(route_arguments("\"/users/$id\"").is_none());
        assert!(route_arguments("'/a', name: self::NAME").is_none());
        assert!(route_arguments("'/a', methods: [Request::METHOD_GET]").is_none());
        assert!(route_arguments("'/a', methods: ['G E T']").is_none());
        assert!(
            route_arguments("\"/\\x61\"").is_none(),
            "escape sequences are not published undecoded"
        );
        let methods = |count: usize| {
            let mut list = vec!["'GET'"; count];
            list.push("self::METHOD");
            format!("'/a', methods: [{}]", list.join(", "))
        };
        assert!(
            route_arguments(&methods(MAX_ROUTE_METHODS - 1)).is_none(),
            "a computed method within the bound is rejected"
        );
        assert!(
            route_arguments(&methods(MAX_ROUTE_METHODS)).is_none(),
            "a computed method past the bound is rejected, not truncated away"
        );
        let literal = ["'GET'"; MAX_ROUTE_METHODS].join(", ");
        assert!(route_arguments(&format!("'/a', methods: [{literal}]")).is_some());
    }

    #[test]
    fn class_methods_merge_before_route_methods_without_repeats() {
        let globals = RouteAttribute {
            methods: vec!["get", "HEAD"],
            ..RouteAttribute::default()
        };
        let route = RouteAttribute {
            methods: vec!["GET", "post"],
            ..RouteAttribute::default()
        };
        assert_eq!(merged_methods(Some(&globals), &route), "GET|HEAD|POST");
        assert_eq!(merged_methods(None, &RouteAttribute::default()), "ANY");
    }
}
