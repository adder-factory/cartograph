//! R structural extraction.
//!
//! Restores the v1 R extractor: R functions are anonymous values named by the
//! left-hand side of `<-`, `=`, or `<<-`, so inline lambdas declare nothing;
//! other top-level assignments are constants and assignments inside any
//! function are variables; `library`/`require` attach a package and `source`
//! evaluates a literal file path, both retained as imports next to their calls.

use cartograph_domain::{ReferenceKind, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ImportBindingKind, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingReference, references,
    script_support::{
        LoadImport, MAX_SCRIPT_NAME_BYTES, OwnerScope, bounded_name, bounded_reference_name,
        bounded_text, emit_load_import, literal_free_node_signature, plain_symbol, with_owner,
    },
    syntax::named_children,
};

/// Leftward assignment operators that bind their left-hand identifier.
const ASSIGNMENT_OPERATORS: [&str; 3] = ["<-", "=", "<<-"];
/// Operators that qualify a callee by a package (`pkg::f`) or object (`obj$f`).
const QUALIFIED_CALLEES: [&str; 2] = ["namespace_operator", "extract_operator"];
/// Maximum nesting followed through a qualified callee.
const MAX_CALLEE_DEPTH: usize = 16;
/// Opening and closing delimiters accepted by R 4.0 raw strings.
const RAW_STRING_DELIMITERS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
/// Named arguments that carry the package or file a load names.
const LOAD_TARGET_ARGUMENTS: [&str; 2] = ["package", "file"];
/// Named argument that turns a load into a documentation lookup.
const HELP_ARGUMENT: &str = "help";
/// Named argument that makes a bare-identifier target an evaluated variable.
const CHARACTER_ONLY_ARGUMENT: &str = "character.only";
/// The only `character.only =` value that keeps a bare identifier a package
/// name: the reserved constant `FALSE` (`F` is an ordinary, reassignable variable).
const CHARACTER_ONLY_DISABLED: &str = "FALSE";

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "program" if node.parent().is_none() => {
            builder.visit_named_children(node, depth)?;
            Ok(true)
        }
        "binary_operator" => visit_assignment(builder, node, depth),
        "function_definition" => {
            // An inline lambda declares nothing; its body belongs to the
            // enclosing owner.
            within_function_value(builder, |builder| builder.visit_named_children(node, depth))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "call" {
        return Ok(());
    }
    let Some(callee) = node.child_by_field_name("function") else {
        return Ok(());
    };
    // A computed callee (`fetch(1)$run()`) has no stable name; its inner calls
    // are captured on their own.
    let Some(name) = static_callee_name(builder, callee, 0)
        .map(|name| bounded_reference_name(builder, &name))
        .transpose()?
        .flatten()
    else {
        return Ok(());
    };
    let load = LoadKind::classify(&name);
    let owner = builder.owners.last().cloned();
    references::push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: ReferenceKind::Calls,
            node: callee,
        },
    )?;
    match load {
        Some(kind) => capture_load(builder, node, kind),
        None => Ok(()),
    }
}

/// `f`, `pkg::f`, or `obj$member$f` rebuilt from identifier parts
/// (parentheses and spacing dropped), or `None` for any computed part.
fn static_callee_name(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Option<String> {
    if depth > MAX_CALLEE_DEPTH {
        return None;
    }
    let next = depth.saturating_add(1);
    match node.kind() {
        "identifier" => Some(builder.context.text(node).trim().to_owned()),
        "parenthesized_expression" => static_callee_name(builder, sole_parenthesized(node)?, next),
        kind if QUALIFIED_CALLEES.contains(&kind) => {
            let base = static_callee_name(builder, node.child_by_field_name("lhs")?, next)?;
            let operator = node.child_by_field_name("operator")?.kind();
            let member = node
                .child_by_field_name("rhs")
                .filter(|member| member.kind() == "identifier")?;
            let member = builder.context.text(member).trim();
            (base.len().saturating_add(member.len()) < MAX_SCRIPT_NAME_BYTES)
                .then(|| format!("{base}{operator}{member}"))
        }
        _ => None,
    }
}

/// The single expression inside parentheses.
fn sole_parenthesized(node: Node<'_>) -> Option<Node<'_>> {
    let mut inner = named_children(node);
    match (inner.next(), inner.next()) {
        (Some(expression), None) => Some(expression),
        _ => None,
    }
}

/// `node` with any enclosing parentheses removed.
fn without_parentheses(mut node: Node<'_>) -> Node<'_> {
    for _ in 0..MAX_CALLEE_DEPTH {
        match (node.kind(), sole_parenthesized(node)) {
            ("parenthesized_expression", Some(inner)) => node = inner,
            _ => break,
        }
    }
    node
}

fn visit_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let assigns = is_assignment(node);
    let (Some(lhs), Some(rhs)) = (
        node.child_by_field_name("lhs"),
        node.child_by_field_name("rhs"),
    ) else {
        return Ok(false);
    };
    if !assigns || lhs.kind() != "identifier" {
        return Ok(false);
    }
    let Some(name) = bounded_name(builder, lhs)? else {
        return Ok(false);
    };
    // Scope comes from enclosing functions, not from an initializer owner:
    // `a <- b <- 1` declares both at top level.
    let top_level = builder.script.r_function_depth == 0;
    let chained = is_assignment(without_parentheses(rhs));
    let mut pending = if rhs.kind() == "function_definition" {
        function_symbol(
            builder,
            FunctionAssignment {
                assignment: node,
                definition: rhs,
            },
            name.clone(),
        )?
    } else if top_level {
        plain_symbol(SymbolKind::Constant, name.clone(), node)
    } else {
        plain_symbol(SymbolKind::Variable, name.clone(), node)
    };
    if chained {
        // `a <- b <- 1`: `b` and its value are analysed as their own symbol,
        // so `a` is analysed through its target alone and a long chain stays
        // linear rather than being re-analysed once per link.
        pending.structural_node = lhs;
    }
    pending.export = SymbolExportFlags::named(top_level);
    let kind = pending.kind;
    let id = builder.emit_symbol(pending)?;
    let scope = OwnerScope {
        id: &id,
        kind,
        name: &name,
    };
    if chained {
        // `a <- b <- 1` declares `b` beside `a`, not inside it.
        builder.visit(rhs, depth.saturating_add(1))?;
        return Ok(true);
    }
    with_owner(builder, scope, |builder| {
        if kind == SymbolKind::Function {
            within_function_value(builder, |builder| {
                builder.visit_named_children(rhs, depth.saturating_add(1))
            })
        } else {
            builder.visit(rhs, depth.saturating_add(1))
        }
    })?;
    Ok(true)
}

/// Whether `node` is a leftward assignment (`<-`, `=`, `<<-`).
fn is_assignment(node: Node<'_>) -> bool {
    node.kind() == "binary_operator"
        && node
            .child_by_field_name("operator")
            .is_some_and(|operator| ASSIGNMENT_OPERATORS.contains(&operator.kind()))
}

/// An assignment whose right-hand side is a function definition.
#[derive(Clone, Copy)]
struct FunctionAssignment<'tree> {
    assignment: Node<'tree>,
    definition: Node<'tree>,
}

/// The function declared by `input`, with its literal-free parameter list as
/// the signature (`(a, b)`).
fn function_symbol<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    input: FunctionAssignment<'tree>,
    name: String,
) -> Result<super::PendingSymbol<'tree>, ExtractError> {
    let FunctionAssignment {
        assignment,
        definition,
    } = input;
    let mut pending = plain_symbol(SymbolKind::Function, name, assignment);
    pending.structural_node = definition;
    pending.body_node = Some(definition);
    pending.signature = match definition.child_by_field_name("parameters") {
        Some(parameters) => {
            literal_free_node_signature(builder, parameters, builder.context.text(parameters))?
        }
        None => None,
    };
    Ok(pending)
}

/// Run `action` inside a function value (named or anonymous), whose
/// assignments are local to that function even when no symbol owns them.
fn within_function_value(
    builder: &mut ExtractionBuilder<'_, '_>,
    action: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    builder.script.r_function_depth = builder.script.r_function_depth.saturating_add(1);
    let result = action(builder);
    builder.script.r_function_depth = builder.script.r_function_depth.saturating_sub(1);
    result
}

/// Package attachment and file sourcing calls retained as imports.
#[derive(Clone, Copy)]
enum LoadKind {
    /// `library(pkg)` / `require(pkg)`: an installed package, never a project file.
    Package,
    /// `source("path.R")`: a literal path evaluated from the working directory,
    /// which project resolution takes to be the project root.
    Source,
}

impl LoadKind {
    fn classify(callee: &str) -> Option<Self> {
        match callee {
            "library" | "require" => Some(Self::Package),
            "source" => Some(Self::Source),
            _ => None,
        }
    }
}

fn capture_load(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
    kind: LoadKind,
) -> Result<(), ExtractError> {
    let Some(target) = load_argument(builder, call) else {
        return Ok(());
    };
    let module = match (kind, target.value.kind()) {
        (LoadKind::Package, "identifier") if !target.character_only => {
            bounded_name(builder, target.value)?
        }
        (_, "string") => string_value(builder, target.value)?,
        _ => None,
    };
    let Some(display) = module else {
        return Ok(());
    };
    let specifier = builder.context.copy_text(&display)?;
    let (binding_kind, namespace) = match kind {
        // `pkg::name` addresses the attached package explicitly.
        LoadKind::Package => (
            ImportBindingKind::IncludeSystem,
            Some(builder.context.copy_text(&display)?),
        ),
        LoadKind::Source => (ImportBindingKind::Namespace, None),
    };
    emit_load_import(
        builder,
        LoadImport {
            site: call,
            display,
            specifier,
            namespace,
            kind: binding_kind,
        },
    )
}

/// The value naming a load's package or file.
#[derive(Clone, Copy)]
struct LoadTarget<'tree> {
    value: Node<'tree>,
    /// Whether `character.only =` (other than `FALSE`) makes a bare
    /// identifier an evaluated variable rather than the package name.
    character_only: bool,
}

/// The first positional argument or a `package =`/`file =` argument of a
/// load. `help =` only shows documentation, so such a call loads nothing.
fn load_argument<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'tree>,
) -> Option<LoadTarget<'tree>> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let mut target = None;
    let mut character_only = false;
    for argument in arguments.children_by_field_name("argument", &mut cursor) {
        let name = argument
            .child_by_field_name("name")
            .map(|name| builder.context.text(name).trim());
        match name {
            Some(HELP_ARGUMENT) => return None,
            Some(CHARACTER_ONLY_ARGUMENT) => {
                character_only = argument.child_by_field_name("value").is_none_or(|value| {
                    builder.context.text(value).trim() != CHARACTER_ONLY_DISABLED
                });
            }
            Some(name) if LOAD_TARGET_ARGUMENTS.contains(&name) => target = Some(argument),
            None if target.is_none() => target = Some(argument),
            _ => {}
        }
    }
    let value = target?.child_by_field_name("value")?;
    Some(LoadTarget {
        value,
        character_only,
    })
}

/// The literal value of an escape-free R string, including R 4.0 raw strings
/// such as `r"(...)"`, `R"[...]"`, `r"{...}"`, and `r"-(...)-"`.
fn string_value(
    builder: &ExtractionBuilder<'_, '_>,
    string: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut parts = named_children(string);
    let value = match (parts.next(), parts.next()) {
        (Some(content), None) if content.kind() == "string_content" => {
            Some(builder.context.text(content))
        }
        (None, None) => raw_string_body(builder.context.text(string)),
        _ => None,
    };
    match value {
        Some(value) if !value.contains('\\') => bounded_text(builder, value),
        _ => Ok(None),
    }
}

fn raw_string_body(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(['r', 'R'])?;
    let quote = rest
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))?;
    let rest = rest.get(quote.len_utf8()..)?.strip_suffix(quote)?;
    let dashes = rest.len() - rest.trim_start_matches('-').len();
    let fence = rest.get(..dashes)?;
    let body_and_close = rest.get(dashes..)?.strip_suffix(fence)?;
    let open = body_and_close.chars().next()?;
    let (_, close) = RAW_STRING_DELIMITERS
        .iter()
        .find(|(candidate, _)| *candidate == open)?;
    body_and_close.strip_prefix(open)?.strip_suffix(*close)
}

#[cfg(test)]
mod tests {
    use super::raw_string_body;

    #[test]
    fn raw_strings_require_matching_delimiters_and_dashes() {
        assert_eq!(raw_string_body("r\"(helpers.R)\""), Some("helpers.R"));
        assert_eq!(raw_string_body("R'[pkg]'"), Some("pkg"));
        assert_eq!(raw_string_body("r\"-(a)b)-\""), Some("a)b"));
        assert_eq!(raw_string_body("r\"(unclosed]\""), None);
        assert_eq!(raw_string_body("r\"--(a)-\""), None);
        assert_eq!(raw_string_body("\"plain\""), None);
    }
}
