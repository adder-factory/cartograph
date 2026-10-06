//! Nix structural extraction.
//!
//! Restores the v1 Nix extractor: a binding is a function when its value is a
//! lambda and a constant otherwise, named by its full attribute path
//! (`a.b.c`); `inherit` and `inherit (source)` declare one constant per
//! attribute and reference the source expression; an application calls its
//! head (`pkgs.stdenv.mkDerivation`), and `import <path>` is an import.

use cartograph_domain::{ReferenceKind, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, ImportBindingKind};

use super::{
    ExtractionBuilder, PendingReference, references,
    script_support::{
        LoadImport, MAX_SCRIPT_NAME_BYTES, OwnerScope, bounded_text, emit_load_import,
        is_relative_specifier, literal_free_node_signature, literal_free_signature, plain_symbol,
        with_owner,
    },
    syntax::named_children,
};

/// Maximum nested selections, parentheses, and applications folded into one call name.
const MAX_TARGET_DEPTH: usize = 32;
/// Characters other than ASCII letters and digits allowed in a Nix identifier.
const IDENTIFIER_PUNCTUATION: [char; 3] = ['_', '\'', '-'];
/// Expressions whose target is named by their identifiers and attribute paths.
const NAMED_TARGETS: [&str; 3] = [
    "variable_expression",
    "select_expression",
    "apply_expression",
];
/// Attribute-path segment computed at evaluation time (`${system}`).
const INTERPOLATION: &str = "interpolation";
/// Path syntax accepted as a literal `import` target.
const PATH_EXPRESSIONS: [&str; 3] = ["path_expression", "hpath_expression", "spath_expression"];

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "binding" => visit_binding(builder, node, depth),
        "inherit" | "inherit_from" => visit_inherit(builder, node, depth).map(|()| true),
        _ => Ok(false),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "apply_expression" {
        return Ok(());
    }
    capture_import(builder, node)?;
    let curried = applied_again(node);
    let Some(head) = node.child_by_field_name("function") else {
        return Ok(());
    };
    if curried {
        return Ok(());
    }
    let Some(name) = target_name(builder, head, 0)? else {
        return Ok(());
    };
    let owner = builder.owners.last().cloned();
    references::push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: ReferenceKind::Calls,
            node: head,
        },
    )
}

/// Whether `application` is the function of an enclosing application, looking
/// through parentheses: `f a b` and `(f a) b` are one call of `f`.
fn applied_again(application: Node<'_>) -> bool {
    let mut child = application;
    for _ in 0..MAX_TARGET_DEPTH {
        let Some(parent) = child.parent() else {
            return false;
        };
        match parent.kind() {
            "parenthesized_expression" => child = parent,
            "apply_expression" => return parent.child_by_field_name("function") == Some(child),
            _ => return false,
        }
    }
    false
}

fn visit_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let (Some(attrpath), Some(expression)) = (
        node.child_by_field_name("attrpath"),
        node.child_by_field_name("expression"),
    ) else {
        return Ok(false);
    };
    let Some(name) = attrpath_name(
        builder,
        AttrPath {
            node: attrpath,
            quoted: QuotedSegments::Accepted,
        },
    )?
    else {
        return Ok(false);
    };
    let function = expression.kind() == "function_expression";
    let kind = if function {
        SymbolKind::Function
    } else {
        SymbolKind::Constant
    };
    let mut pending = plain_symbol(kind, name.clone(), node);
    if function {
        pending.structural_node = expression;
        pending.body_node = Some(expression);
        pending.signature = lambda_signature(builder, expression)?;
    }
    let id = builder.emit_symbol(pending)?;
    let scope = OwnerScope {
        id: &id,
        kind,
        name: &name,
    };
    with_owner(builder, scope, |builder| {
        builder.visit(expression, depth.saturating_add(1))
    })?;
    Ok(true)
}

/// The lambda head before its body (`x` or `{ a, b }`) without the trailing colon.
fn lambda_signature(
    builder: &ExtractionBuilder<'_, '_>,
    lambda: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(body) = lambda.child_by_field_name("body") else {
        return Ok(None);
    };
    let head = builder
        .context
        .source()
        .get(lambda.start_byte()..body.start_byte())
        .unwrap_or_default()
        .trim()
        .trim_end_matches(':')
        .trim_end();
    match lambda.child_by_field_name("formals") {
        Some(formals) => literal_free_node_signature(builder, formals, head),
        None => literal_free_signature(builder, head),
    }
}

fn visit_inherit(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if let Some(attrs) = node.child_by_field_name("attrs") {
        let mut cursor = attrs.walk();
        let attributes = attrs
            .children_by_field_name("attr", &mut cursor)
            .collect::<Vec<_>>();
        let sole = attributes.len() == 1;
        for attribute in attributes {
            let Some(name) = attribute_name(builder, attribute)? else {
                continue;
            };
            // A sole constant's structure is the whole `inherit`; in a list
            // each constant is analysed through its own attribute, so a wide
            // list is not re-analysed once per attribute.
            let mut pending = plain_symbol(SymbolKind::Constant, name, attribute);
            if sole {
                pending.structural_node = node;
            }
            pending.doc_anchor = node;
            builder.emit_symbol(pending)?;
        }
    }
    let Some(source) = node.child_by_field_name("expression") else {
        return Ok(());
    };
    if let Some(name) = target_name(builder, source, 0)? {
        let owner = builder.owners.last().cloned();
        references::push_reference(
            builder,
            PendingReference {
                owner,
                name,
                kind: ReferenceKind::References,
                node: source,
            },
        )?;
    }
    builder.visit(source, depth.saturating_add(1))
}

/// `import <path>` where the application's head is the `import` builtin itself.
fn capture_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    application: Node<'_>,
) -> Result<(), ExtractError> {
    let imports = application
        .child_by_field_name("function")
        .is_some_and(|head| is_import_builtin(builder, head));
    let Some(argument) = application.child_by_field_name("argument") else {
        return Ok(());
    };
    if !imports {
        return Ok(());
    }
    let path = if PATH_EXPRESSIONS.contains(&argument.kind())
        && named_children(argument).all(|part| part.kind() == "path_fragment")
    {
        bounded_text(builder, builder.context.text(argument))?
    } else if argument.kind() == "string_expression" {
        plain_string(builder, argument)?
    } else {
        None
    };
    let Some(display) = path else {
        return Ok(());
    };
    let relative_path = argument.kind() == "path_expression" && !display.starts_with('/');
    let (kind, specifier) = if is_relative_specifier(&display) {
        (
            ImportBindingKind::Namespace,
            builder.context.copy_text(&display)?,
        )
    } else if relative_path {
        // A Nix path literal without a leading `/` is relative to the file.
        (
            ImportBindingKind::Namespace,
            builder.context.copy_text(&format!("./{display}"))?,
        )
    } else {
        (
            ImportBindingKind::IncludeSystem,
            builder.context.copy_text(&display)?,
        )
    };
    emit_load_import(
        builder,
        LoadImport {
            site: application,
            display,
            specifier,
            namespace: None,
            kind,
        },
    )
}

/// `import` or `builtins.import` as an application head.
fn is_import_builtin(builder: &ExtractionBuilder<'_, '_>, head: Node<'_>) -> bool {
    matches!(head.kind(), "variable_expression" | "select_expression")
        && matches!(
            builder.context.text(head).trim(),
            "import" | "builtins.import"
        )
}

/// The dotted name an application or inherit source refers to.
fn target_name(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<Option<String>, ExtractError> {
    if depth > MAX_TARGET_DEPTH {
        return Ok(None);
    }
    let next = depth.saturating_add(1);
    match node.kind() {
        "variable_expression" => match node.child_by_field_name("name") {
            Some(name) => identifier_name(builder, name),
            None => Ok(None),
        },
        "parenthesized_expression" => match node.child_by_field_name("expression") {
            Some(inner) => target_name(builder, inner, next),
            None => Ok(None),
        },
        "apply_expression" => match node.child_by_field_name("function") {
            Some(head) => target_name(builder, head, next),
            None => Ok(None),
        },
        "select_expression" => selection_name(builder, node, next),
        _ => Ok(None),
    }
}

/// `base.attr.path` for a selection whose selected path is all identifiers.
///
/// A selected path that cannot be named (`lib.${x}`, `pkgs."name"`) names
/// nothing rather than falling back to its base, which would claim a call of
/// the attribute set itself; quoted segments are string literals and never
/// enter a reference name.
fn selection_name(
    builder: &ExtractionBuilder<'_, '_>,
    selection: Node<'_>,
    depth: usize,
) -> Result<Option<String>, ExtractError> {
    let Some(attribute) = selection
        .child_by_field_name("attrpath")
        .map(|attrpath| {
            attrpath_name(
                builder,
                AttrPath {
                    node: attrpath,
                    quoted: QuotedSegments::Rejected,
                },
            )
        })
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Some(expression) = selection.child_by_field_name("expression") else {
        return Ok(Some(attribute));
    };
    match target_name(builder, expression, depth)? {
        Some(base) => nix_name(builder, &format!("{base}.{attribute}")),
        // A base that names a value but could not be named (`(pkgs."x").run`)
        // leaves the selection unnamed; only a base that is not a name at all
        // (`{ ... }.run`, `(let ... in x).run`) keeps v1's attribute-only name.
        None if names_a_value(expression) => Ok(None),
        None => Ok(Some(attribute)),
    }
}

/// Whether `node`, inside any parentheses, is a variable, selection, or
/// application: syntax whose target has a name when every part is nameable.
/// Parentheses nested beyond the naming depth count as a name, so the caller
/// abstains rather than inventing an attribute-only target.
fn names_a_value(node: Node<'_>) -> bool {
    let mut current = node;
    for _ in 0..MAX_TARGET_DEPTH {
        match current.kind() {
            "parenthesized_expression" => match current.child_by_field_name("expression") {
                Some(inner) => current = inner,
                None => return false,
            },
            kind => return NAMED_TARGETS.contains(&kind),
        }
    }
    true
}

/// Whether quoted (`"name"`) attribute segments may contribute to a name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QuotedSegments {
    /// Declared attribute names keep v1's quoted keys (`"foo-bar" = 1`) and,
    /// like v1, leave interpolated segments out (`packages.${system}.default`
    /// declares `packages.default`).
    Accepted,
    /// Reference names are built from identifiers only.
    Rejected,
}

/// One attribute path and how its quoted segments are treated.
#[derive(Clone, Copy)]
struct AttrPath<'tree> {
    node: Node<'tree>,
    quoted: QuotedSegments,
}

/// Attribute path segments joined with `.`, or `None` when the path names
/// nothing. A declared path skips its interpolated segments; any other dynamic
/// segment (and, in a reference, any quoted one) leaves the path unnamed.
fn attrpath_name(
    builder: &ExtractionBuilder<'_, '_>,
    path: AttrPath<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut cursor = path.node.walk();
    let attributes = path
        .node
        .children_by_field_name("attr", &mut cursor)
        .collect::<Vec<_>>();
    let mut name = String::new();
    for attribute in attributes {
        if path.quoted == QuotedSegments::Accepted && attribute.kind() == INTERPOLATION {
            continue;
        }
        if path.quoted == QuotedSegments::Rejected && attribute.kind() != "identifier" {
            return Ok(None);
        }
        let Some(segment) = attribute_name(builder, attribute)? else {
            return Ok(None);
        };
        if name.len().saturating_add(segment.len()) >= MAX_SCRIPT_NAME_BYTES {
            return Ok(None);
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&segment);
    }
    if name.is_empty() {
        return Ok(None);
    }
    nix_name(builder, &name)
}

/// One attribute name: an identifier, or the content of a plain quoted key.
fn attribute_name(
    builder: &ExtractionBuilder<'_, '_>,
    attribute: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    match attribute.kind() {
        "identifier" => identifier_name(builder, attribute),
        "string_expression" => plain_string(builder, attribute),
        _ => Ok(None),
    }
}

/// A grammar-level Nix identifier, which may contain `'` and `-`
/// (`foldl'`, `mapAttrs'`, `go-modules`).
fn identifier_name(
    builder: &ExtractionBuilder<'_, '_>,
    identifier: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let text = builder.context.text(identifier).trim();
    let valid = text.chars().all(|character| {
        character.is_ascii_alphanumeric() || IDENTIFIER_PUNCTUATION.contains(&character)
    });
    if !valid {
        return Ok(None);
    }
    nix_name(builder, text)
}

/// `name` as an owned, bounded, single-line name free of double quotes and
/// backticks; `'` is an identifier character in Nix, not a quote.
fn nix_name(
    builder: &ExtractionBuilder<'_, '_>,
    name: &str,
) -> Result<Option<String>, ExtractError> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_SCRIPT_NAME_BYTES
        || name
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | '`'))
    {
        return Ok(None);
    }
    builder.context.copy_text(name).map(Some)
}

/// The text of a string made of literal fragments only (no interpolation).
fn plain_string(
    builder: &ExtractionBuilder<'_, '_>,
    string: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut parts = named_children(string);
    match (parts.next(), parts.next()) {
        (Some(fragment), None) if fragment.kind() == "string_fragment" => {
            let value = builder.context.text(fragment);
            if super::specifier_safety::specifier_may_carry_credential(value) {
                return Ok(None);
            }
            bounded_text(builder, value)
        }
        _ => Ok(None),
    }
}
