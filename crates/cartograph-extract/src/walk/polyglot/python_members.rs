//! Python module bindings, decorators, static methods, interface bases, and
//! annotated type consumers.
//!
//! - A module-scope `NAME = value` (including inside a module-level `if` or
//!   `try`) declares a variable. Only the first assignment of a name declares
//!   it; the declared variable also owns the values and annotations of later
//!   module-scope rebinds (`else: NAME: Other = make_b()`). Class-body and
//!   function-local assignments, chained targets after the first, and tuple,
//!   attribute, or subscript targets declare nothing.
//! - An annotated assignment anywhere else (`repo: Repo` in a class body,
//!   `local: Local = make()` in a function) is a `TypeOf` use by its owner.
//! - `@staticmethod` marks a static member; each decorator of a function or
//!   class is a `Decorates` reference from the decorated symbol, named by its
//!   final segment (`@app.route(..)` decorates `route`).
//! - A `Protocol`, `ABC`, or `ABCMeta` base is an interface contract the class
//!   implements; other bases are extended.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::Node;

use super::{
    OwnedBody, python_exported,
    type_targets::{NamedTarget, capture_declared_types, emit_leaf_reference},
    visit_owned_body,
};
use crate::{
    ExtractError, SymbolExportFlags,
    walk::{ExtractionBuilder, PendingSymbol, safe_assignment_signature, syntax::named_children},
};

/// Base classes whose subclasses declare an interface contract (PEP 544
/// `Protocol` and the `abc` module) rather than inherit an implementation.
const PYTHON_INTERFACE_BASES: [&str; 3] = ["Protocol", "ABC", "ABCMeta"];
/// Deepest dotted decorator path (`a.b.c.d`) resolved to a name.
const MAX_DECORATOR_PATH_DEPTH: usize = 16;
/// The throwaway binding, which never declares a variable.
const DISCARDED_BINDING: &str = "_";
/// The builtin decorator that makes a method static.
const STATIC_METHOD_DECORATOR: &str = "staticmethod";

/// Reference kind for one base-class expression: implements for an interface
/// marker matched by its final segment (`typing.Protocol`), extends otherwise.
pub(super) fn base_reference_kind(base: &str) -> ReferenceKind {
    let terminal = base.rsplit('.').next().unwrap_or(base).trim();
    if PYTHON_INTERFACE_BASES.contains(&terminal) {
        ReferenceKind::Implements
    } else {
        ReferenceKind::Extends
    }
}

/// Whether a function is decorated with the bare `@staticmethod`; a call
/// such as `@staticmethod(wrap)` decorates with whatever the call returns.
pub(super) fn is_static_method(builder: &ExtractionBuilder<'_, '_>, function: Node<'_>) -> bool {
    decorators(function).any(|decorator| {
        named_children(decorator)
            .find(|expression| !expression.is_extra())
            .filter(|expression| expression.kind() == "identifier")
            .is_some_and(|expression| {
                builder.context.text(expression).trim() == STATIC_METHOD_DECORATOR
            })
    })
}

/// Record each decorator of `definition` as decorating `owner`.
pub(super) fn capture_decorators(
    builder: &mut ExtractionBuilder<'_, '_>,
    definition: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(());
    }
    for decorator in decorators(definition) {
        builder.context.ensure_active()?;
        let Some(path) = decorator_target(decorator) else {
            continue;
        };
        let Some(leaf) = decorator_leaf(path, 0) else {
            continue;
        };
        emit_leaf_reference(
            builder,
            Some(owner.clone()),
            NamedTarget { path, leaf }.reference(ReferenceKind::Decorates),
        )?;
    }
    Ok(())
}

/// Declare a module-scope variable, or record an annotation's types for the
/// enclosing class or function. Returns whether the assignment was consumed.
pub(super) fn visit_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(false);
    }
    let Some(owner) = builder.owners.last().cloned() else {
        return visit_module_assignment(builder, node, depth);
    };
    if let Some(annotation) = node.child_by_field_name("type") {
        capture_declared_types(builder, annotation, &owner)?;
    }
    Ok(false)
}

fn visit_module_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(target) = node
        .child_by_field_name("left")
        .filter(|target| target.kind() == "identifier")
    else {
        return Ok(false);
    };
    let name = builder.context.owned_text(target)?;
    if name == DISCARDED_BINDING {
        return Ok(false);
    }
    let owner = match builder.native_scope_symbols.get(&name) {
        Some(Some((declared, _))) => declared.clone(),
        _ => declare_module_variable(builder, node, target)?,
    };
    if let Some(annotation) = node.child_by_field_name("type") {
        capture_declared_types(builder, annotation, &owner)?;
    }
    // The variable owns its annotation's other uses and its value's.
    for body in [
        node.child_by_field_name("type"),
        node.child_by_field_name("right"),
    ]
    .into_iter()
    .flatten()
    {
        visit_owned_body(
            builder,
            OwnedBody {
                owner: owner.clone(),
                qualifier: builder.context.copy_text(&name)?,
                body,
                depth,
            },
        )?;
    }
    Ok(true)
}

/// Declare the module variable an assignment's first binding of `target` names.
fn declare_module_variable(
    builder: &mut ExtractionBuilder<'_, '_>,
    assignment: Node<'_>,
    target: Node<'_>,
) -> Result<SymbolId, ExtractError> {
    let value = assignment.child_by_field_name("right");
    let signature = value
        .map(|value| safe_assignment_signature(builder, value))
        .transpose()?
        .flatten();
    let name = builder.context.owned_text(target)?;
    let id = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Variable,
        name,
        span_node: assignment,
        structural_node: assignment,
        doc_anchor: assignment.parent().unwrap_or(assignment),
        body_node: value,
        declaration_only: false,
        signature,
        export: SymbolExportFlags::new(python_exported(builder, target), false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    builder.native_scope_symbols.insert(
        builder.context.owned_text(target)?,
        Some((id.clone(), SymbolKind::Variable)),
    );
    Ok(id)
}

/// Decorators of a definition wrapped in a `decorated_definition`.
fn decorators(definition: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    definition
        .parent()
        .filter(|parent| parent.kind() == "decorated_definition")
        .into_iter()
        .flat_map(named_children)
        .filter(|child| child.kind() == "decorator")
}

/// The decorator's static dotted path, unwrapping a call (`@route('/x')`).
fn decorator_target(decorator: Node<'_>) -> Option<Node<'_>> {
    let expression = named_children(decorator).next()?;
    let target = if expression.kind() == "call" {
        expression.child_by_field_name("function")?
    } else {
        expression
    };
    decorator_leaf(target, 0).map(|_| target)
}

/// Final name of a static dotted path (`a.b.route` -> `route`), or `None`
/// when any part of the path is not a plain name.
fn decorator_leaf(path: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > MAX_DECORATOR_PATH_DEPTH {
        return None;
    }
    match path.kind() {
        "identifier" => Some(path),
        "attribute" => {
            decorator_leaf(path.child_by_field_name("object")?, depth.saturating_add(1))?;
            path.child_by_field_name("attribute")
        }
        _ => None,
    }
}
