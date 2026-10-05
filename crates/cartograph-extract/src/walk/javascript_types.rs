//! TypeScript type consumers inside executable bodies.
//!
//! A type named only at a use site — call-site type arguments
//! (`call<Payload>(v)`, `new Map<K, V>()`), `as`/`satisfies` assertions, and
//! the parameter and return annotations of anonymous callbacks
//! (`map((x: Row): Out => ..)`) —
//! becomes a [`ReferenceKind::TypeOf`] owned by the enclosing symbol, so a
//! type whose only consumer is a body still has an incoming edge (v1
//! `captureTsConsumerTypePosition`).
//!
//! A non-callable variable declarator already records every type named
//! anywhere in its declaration, so consumers inside such an initializer are
//! left to that capture instead of being recorded twice. A type parameter
//! that the callback or an enclosing function or class introduces
//! (`<Payload>(x: Payload) => x`) names no declaration and is not recorded.

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    ExtractionBuilder, PendingReference, javascript_scopes, references,
    syntax::{descendants_including_root, named_children},
};

/// Record the type consumers rooted at `node` for the current owner.
pub(super) fn capture_type_consumers(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if !matches!(
        builder.context.snapshot.language(),
        SourceLanguage::TypeScript | SourceLanguage::Tsx
    ) {
        return Ok(());
    }
    let Some(owner) = builder.owners.last().cloned() else {
        return Ok(());
    };
    let consumer = match node.kind() {
        "type_arguments" => node
            .parent()
            .filter(|parent| matches!(parent.kind(), "call_expression" | "new_expression"))
            .map(|_| ConsumerRoots::Single(node)),
        "as_expression" | "satisfies_expression" => {
            references::asserted_type(node).map(ConsumerRoots::Single)
        }
        "arrow_function" | "function_expression" | "generator_function" => {
            Some(ConsumerRoots::Callback(node))
        }
        _ => None,
    };
    let Some(consumer) = consumer else {
        return Ok(());
    };
    if declarator_captured(builder) {
        return Ok(());
    }
    capture_roots(builder, consumer, &owner)
}

/// Where the consumed types of one node live.
#[derive(Clone, Copy)]
enum ConsumerRoots<'tree> {
    /// One type subtree.
    Single(Node<'tree>),
    /// The parameter and return annotations of an anonymous callback.
    Callback(Node<'tree>),
}

/// Record `type_of` references for every type named under the roots.
fn capture_roots(
    builder: &mut ExtractionBuilder<'_, '_>,
    roots: ConsumerRoots<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let types = |root| ConsumerTypes {
        root,
        owner,
        kind: ReferenceKind::TypeOf,
    };
    let callback = match roots {
        ConsumerRoots::Single(root) => return capture_consumer_types(builder, types(root)),
        ConsumerRoots::Callback(callback) => callback,
    };
    if let Some(parameters) = callback.child_by_field_name("parameters") {
        for parameter in named_children(parameters) {
            builder.context.ensure_active()?;
            if let Some(annotation) = parameter.child_by_field_name("type") {
                capture_consumer_types(builder, types(annotation))?;
            }
        }
    }
    match callback.child_by_field_name("return_type") {
        Some(return_type) => capture_consumer_types(builder, types(return_type)),
        None => Ok(()),
    }
}

/// One type subtree whose names a symbol consumes.
#[derive(Clone, Copy)]
pub(super) struct ConsumerTypes<'tree, 'owner> {
    pub(super) root: Node<'tree>,
    pub(super) owner: &'owner SymbolId,
    /// `type_of`, or `returns` for a return annotation.
    pub(super) kind: ReferenceKind,
}

/// Record references for the types named under a consumed type subtree,
/// except a type parameter that the callback itself or an enclosing
/// function or class introduces (`<Payload>(x: Payload) => ..` names no
/// `Payload` declaration) and a bare name a nearer or flattened declaration
/// would capture; a qualified name is recorded whole.
pub(super) fn capture_consumer_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    types: ConsumerTypes<'_, '_>,
) -> Result<(), ExtractError> {
    let ConsumerTypes { root, owner, kind } = types;
    for target in descendants_including_root(root) {
        builder.context.ensure_active()?;
        if target.kind() == "nested_type_identifier" {
            capture_qualified_type(
                builder,
                ConsumerTypes {
                    root: target,
                    owner,
                    kind,
                },
            )?;
            continue;
        }
        if is_qualified_member(target) {
            // Recorded with its qualifier by `capture_qualified_type`.
            continue;
        }
        if is_bare_type_name(target) && !javascript_scopes::type_name_resolves(builder, target)? {
            continue;
        }
        // A `typeof value` operand resolves as a value read, so it must name
        // what the resolver would bind it to (not a rebound callback
        // parameter's module namesake).
        if is_type_query_operand(target)
            && !javascript_scopes::read_binding(builder, target)?.resolves_by_name()
        {
            continue;
        }
        references::capture_type_node(
            builder,
            references::TypeNodeCapture {
                target,
                owner,
                kind,
            },
        )?;
    }
    Ok(())
}

/// Record a qualified type (`Types.Payload`) under its whole static name, so
/// the resolver binds it through the qualifier (a namespace import) rather
/// than to an unrelated `Payload`. A qualifier spelled with anything but
/// identifiers and dots (comments, whitespace) names nothing.
fn capture_qualified_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    qualified: ConsumerTypes<'_, '_>,
) -> Result<(), ExtractError> {
    let ConsumerTypes {
        root: qualified,
        owner,
        kind,
    } = qualified;
    let text = builder.context.text(qualified);
    if text.is_empty()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'.'))
    {
        return Ok(());
    }
    let name = builder.context.owned_text(qualified)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner.clone()),
            name,
            kind,
            node: qualified,
        },
    )
}

/// Whether a type name is the member of a qualified type (`Payload` in
/// `Types.Payload`).
fn is_qualified_member(node: Node<'_>) -> bool {
    node.kind() == "type_identifier"
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == "nested_type_identifier")
}

/// Whether a node is the identifier a type query reads (`typeof value`).
fn is_type_query_operand(node: Node<'_>) -> bool {
    node.kind() == "identifier"
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == "type_query")
}

/// Whether a node is an unqualified type name (`Payload`), the only form a
/// type parameter or a nearer type declaration can shadow.
fn is_bare_type_name(node: Node<'_>) -> bool {
    node.kind() == "type_identifier" && !is_qualified_member(node)
}

/// Whether the current owner's declaration — a non-callable variable
/// declarator or a type alias, whose value the walker visits under it —
/// already recorded every type named anywhere inside it.
fn declarator_captured(builder: &ExtractionBuilder<'_, '_>) -> bool {
    builder
        .owners
        .last()
        .is_some_and(|owner| builder.javascript.whole_type_owners.contains(owner))
}
