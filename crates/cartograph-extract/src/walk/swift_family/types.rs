//! Swift parameter, return, and stored-property type references.
//!
//! A dotted type keeps its qualification (`Outer.Bar` is the reference
//! `Outer::Bar`, spanning the whole type) so it can never resolve to an
//! unrelated top-level `Bar`. Builtin scalars, in-scope generic parameters
//! (including those of the type an extension reopens), metatype suffixes, and
//! `Swift.`-qualified standard-library types never name project declarations.

use cartograph_domain::{ReferenceKind, SymbolId};
use tree_sitter::Node;

use crate::ExtractError;

use super::super::{
    ExtractionBuilder, PendingReference, references,
    syntax::{children, named_children},
};
use super::unescaped;

/// Syntax levels searched for enclosing generic parameter lists.
const MAXIMUM_GENERIC_SCOPE_DEPTH: usize = 32;
/// Sibling declarations searched, per extended path segment, for the type an
/// extension reopens.
const MAXIMUM_EXTENSION_TARGET_SCAN: usize = 1_024;
/// Type-annotation nesting followed before giving up on deeper references.
const MAXIMUM_TYPE_NESTING: usize = 64;
/// Metatype suffixes (`T.Type`, `P.Protocol`) that are not type names.
const METATYPE_SUFFIXES: [&str; 2] = ["Type", "Protocol"];
/// The attribute list written between `->` and the returned type.
const RETURN_TYPE_ATTRIBUTES: &str = "type_modifiers";
/// The standard-library module qualifier (`Swift.String`).
const STANDARD_LIBRARY_MODULE: &str = "Swift";
/// Standard-library scalar types that never resolve to project declarations.
const SWIFT_BUILTIN_TYPES: &[&str] = &[
    "Any",
    "AnyObject",
    "Bool",
    "Character",
    "Double",
    "Float",
    "Int",
    "Int8",
    "Int16",
    "Int32",
    "Int64",
    "Never",
    "Self",
    "String",
    "UInt",
    "UInt8",
    "UInt16",
    "UInt32",
    "UInt64",
    "Void",
];

/// The declaration whose annotations are captured and the generic parameter
/// names in scope there, which never name project types.
#[derive(Clone, Copy)]
pub(super) struct TypeScope<'scope> {
    pub(super) owner: &'scope SymbolId,
    pub(super) generics: &'scope [&'scope str],
}

/// A type annotation subtree and the relationship it records.
#[derive(Clone, Copy)]
pub(super) struct TypeCapture<'tree, 'scope> {
    root: Node<'tree>,
    kind: ReferenceKind,
    scope: TypeScope<'scope>,
}

impl<'tree, 'scope> TypeCapture<'tree, 'scope> {
    pub(super) const fn type_of(root: Node<'tree>, scope: TypeScope<'scope>) -> Self {
        Self {
            root,
            kind: ReferenceKind::TypeOf,
            scope,
        }
    }

    const fn at(self, root: Node<'tree>) -> Self {
        Self { root, ..self }
    }
}

/// Parameter annotations are consumed types; the annotation after `->` is
/// returned. Attributes written before the returned type (`-> @Sendable (A) -> B`)
/// qualify it and are not themselves the type.
pub(super) fn capture_callable_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    scope: TypeScope<'_>,
) -> Result<(), ExtractError> {
    let mut returns_next = false;
    for child in children(node) {
        builder.context.ensure_active()?;
        if child.kind() == "->" {
            returns_next = true;
            continue;
        }
        if !child.is_named() {
            continue;
        }
        if child.kind() == "parameter" {
            capture_types(builder, TypeCapture::type_of(child, scope))?;
        } else if returns_next && child.kind() != RETURN_TYPE_ATTRIBUTES {
            capture_types(
                builder,
                TypeCapture {
                    root: child,
                    kind: ReferenceKind::Returns,
                    scope,
                },
            )?;
            returns_next = false;
        }
    }
    Ok(())
}

pub(super) fn capture_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: TypeCapture<'_, '_>,
) -> Result<(), ExtractError> {
    capture_type_node(builder, capture, 0)
}

fn capture_type_node(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: TypeCapture<'_, '_>,
    nesting: usize,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    if nesting > MAXIMUM_TYPE_NESTING {
        return Ok(());
    }
    let node = capture.root;
    match node.kind() {
        "user_type" => capture_user_type(builder, capture)?,
        "type_identifier" => capture_named_type(builder, capture, &[node])?,
        _ => {}
    }
    for child in named_children(node) {
        // A user type's own identifiers were captured as one qualified name.
        if node.kind() == "user_type" && child.kind() == "type_identifier" {
            continue;
        }
        capture_type_node(builder, capture.at(child), nesting.saturating_add(1))?;
    }
    Ok(())
}

fn capture_user_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: TypeCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let path = type_path(capture.root);
    capture_named_type(builder, capture, &path)
}

/// Emit one reference for a type path after dropping a metatype suffix. A path
/// rooted at the `Swift` module or a generic parameter, or naming a builtin
/// scalar, emits nothing: it can never denote a project declaration.
fn capture_named_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    capture: TypeCapture<'_, '_>,
    path: &[Node<'_>],
) -> Result<(), ExtractError> {
    let mut path = path;
    if let [rest @ .., suffix] = path
        && !rest.is_empty()
        && METATYPE_SUFFIXES.contains(&unescaped(builder.context.text(*suffix)))
    {
        path = rest;
    }
    let [first, ..] = path else {
        return Ok(());
    };
    let root_name = unescaped(builder.context.text(*first));
    if path
        .iter()
        .any(|segment| unescaped(builder.context.text(*segment)).is_empty())
        || capture.scope.generics.contains(&root_name)
        || is_standard_type_path(path.len(), root_name)
    {
        return Ok(());
    }
    let span = if path.len() == 1 {
        *first
    } else {
        capture.root
    };
    let name = qualified_type_name(builder, path)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(capture.scope.owner.clone()),
            name,
            kind: capture.kind,
            node: span,
        },
    )
}

/// Whether a type path of `length` segments rooted at `root_name` names the
/// standard library: a `Swift.`-qualified path or a bare builtin scalar.
fn is_standard_type_path(length: usize, root_name: &str) -> bool {
    length > 1 && root_name == STANDARD_LIBRARY_MODULE
        || length == 1 && SWIFT_BUILTIN_TYPES.contains(&root_name)
}

fn qualified_type_name(
    builder: &ExtractionBuilder<'_, '_>,
    path: &[Node<'_>],
) -> Result<String, ExtractError> {
    let mut name = String::new();
    for segment in path {
        if !name.is_empty() {
            name.push_str("::");
        }
        name.push_str(unescaped(builder.context.text(*segment)));
    }
    builder.context.copy_text(&name)
}

/// The identifiers of a dotted type (`Outer.Inner` is `[Outer, Inner]`).
pub(super) fn type_path(node: Node<'_>) -> Vec<Node<'_>> {
    if node.kind() == "type_identifier" {
        return vec![node];
    }
    named_children(node)
        .filter(|child| child.kind() == "type_identifier")
        .collect()
}

/// Generic parameter names declared by `node`, its enclosing declarations, and
/// the same-file type an enclosing extension reopens.
pub(super) fn generic_parameters<'source>(
    source: &'source str,
    node: Node<'_>,
) -> Vec<&'source str> {
    let mut names = Vec::new();
    let mut current = Some(node);
    for _ in 0..MAXIMUM_GENERIC_SCOPE_DEPTH {
        let Some(scope) = current else {
            break;
        };
        declared_type_parameters(source, scope, &mut names);
        if super::is_extension(scope) {
            extended_type_parameters(source, scope, &mut names);
        }
        current = scope.parent();
    }
    names
}

fn declared_type_parameters<'source>(
    source: &'source str,
    scope: Node<'_>,
    names: &mut Vec<&'source str>,
) {
    for parameters in named_children(scope).filter(|child| child.kind() == "type_parameters") {
        names.extend(
            named_children(parameters)
                .filter_map(|parameter| {
                    named_children(parameter).find(|child| child.kind() == "type_identifier")
                })
                .filter_map(|name| source.get(name.start_byte()..name.end_byte()))
                .map(unescaped),
        );
    }
}

/// The generic parameters of every same-file declaration on the path an
/// extension reopens: `extension Outer.Inner` sees `Outer`'s parameters as
/// well as `Inner`'s, because a nested type is generic over its outer type's.
fn extended_type_parameters<'source>(
    source: &'source str,
    extension: Node<'_>,
    names: &mut Vec<&'source str>,
) {
    let (Some(path), Some(mut container)) =
        (extension.child_by_field_name("name"), extension.parent())
    else {
        return;
    };
    for segment in type_path(path)
        .into_iter()
        .take(MAXIMUM_GENERIC_SCOPE_DEPTH)
    {
        let Some(extended) = source
            .get(segment.start_byte()..segment.end_byte())
            .map(unescaped)
        else {
            return;
        };
        let Some(declaration) = declared_type(source, container, extended) else {
            return;
        };
        declared_type_parameters(source, declaration, names);
        let Some(body) = declaration.child_by_field_name("body") else {
            return;
        };
        container = body;
    }
}

/// The non-extension type or protocol named `name` among a bounded number of
/// `container`'s direct children.
fn declared_type<'tree>(source: &str, container: Node<'tree>, name: &str) -> Option<Node<'tree>> {
    named_children(container)
        .take(MAXIMUM_EXTENSION_TARGET_SCAN)
        .find(|candidate| {
            matches!(
                candidate.kind(),
                "class_declaration" | "protocol_declaration"
            ) && !super::is_extension(*candidate)
                && candidate
                    .child_by_field_name("name")
                    .and_then(|declared| source.get(declared.start_byte()..declared.end_byte()))
                    .map(unescaped)
                    == Some(name)
        })
}
