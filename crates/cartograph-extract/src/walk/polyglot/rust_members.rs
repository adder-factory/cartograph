//! Rust supertraits, struct field types, and struct-expression construction.
//!
//! - Each named bound of `trait T: A + B<X> + Fn(u8) + for<'a> Fn(&'a u8) + m::C`
//!   is a supertrait `T` extends; lifetimes and `?Sized` name no trait.
//! - The field types of a struct (named or tuple) are `TypeOf` uses by the
//!   struct, once per written type; fields are not separate symbols.
//! - `S { .. }`, `m::P { .. }`, and `m::S::<T> { .. }` instantiate their type;
//!   `Self { .. }` names no declaration of its own.

use cartograph_domain::{ReferenceKind, SymbolId};
use tree_sitter::Node;

use super::type_targets::{NamedTarget, capture_declared_types, emit_leaf_reference};
use crate::{
    ExtractError,
    walk::{ExtractionBuilder, syntax::named_children},
};

/// Deepest generic or higher-ranked wrapping unwrapped to reach a trait or type name.
const MAX_TYPE_WRAPPING_DEPTH: usize = 8;
/// The implementing type's alias, which names no separate declaration.
const SELF_TYPE: &str = "Self";

/// Record the types of a struct's field declaration (or tuple field list) as
/// uses by the struct. The usage walk still visits the fields afterwards, for
/// values their types read (`[u8; LEN]`).
pub(super) fn capture_field_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(());
    }
    let fields = match node.kind() {
        "field_declaration" => node.parent().filter(|list| in_struct(*list)),
        "ordered_field_declaration_list" => Some(node).filter(|list| in_struct(*list)),
        _ => None,
    };
    let (Some(_), Some(owner)) = (fields, builder.owners.last().cloned()) else {
        return Ok(());
    };
    let mut cursor = node.walk();
    for field_type in node.children_by_field_name("type", &mut cursor) {
        capture_declared_types(builder, field_type, &owner)?;
    }
    Ok(())
}

/// Record each named supertrait bound of a trait as a trait it extends.
pub(super) fn capture_supertraits(
    builder: &mut ExtractionBuilder<'_, '_>,
    trait_item: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(());
    }
    let Some(bounds) = trait_item.child_by_field_name("bounds") else {
        return Ok(());
    };
    for bound in named_children(bounds) {
        builder.context.ensure_active()?;
        if let Some(target) = bound_trait(bound, 0) {
            emit_leaf_reference(
                builder,
                Some(owner.clone()),
                target.reference(ReferenceKind::Extends),
            )?;
        }
    }
    Ok(())
}

/// Whether a path is the type of a turbofish struct expression
/// (`m::S::<T> { .. }`), which names it through the instantiation when this
/// pass records optional facts.
pub(super) fn names_instantiated_type(builder: &ExtractionBuilder<'_, '_>, path: Node<'_>) -> bool {
    builder.optional_facts.records()
        && path
            .parent()
            .filter(|parent| parent.kind() == "generic_type_with_turbofish")
            .and_then(|parent| parent.parent())
            .is_some_and(|expression| expression.kind() == "struct_expression")
}

/// Record a struct expression as instantiating its named type.
pub(super) fn capture_struct_expression(
    builder: &mut ExtractionBuilder<'_, '_>,
    expression: Node<'_>,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(());
    }
    let Some(target) = expression
        .child_by_field_name("name")
        .and_then(|name| named_type(name, 0))
    else {
        return Ok(());
    };
    if builder.context.text(target.path).trim() == SELF_TYPE {
        return Ok(());
    }
    emit_leaf_reference(
        builder,
        builder.owners.last().cloned(),
        target.reference(ReferenceKind::Instantiates),
    )
}

/// Whether a field list belongs to a struct item.
fn in_struct(list: Node<'_>) -> bool {
    list.parent()
        .is_some_and(|parent| parent.kind() == "struct_item")
}

/// The trait a bound names: a plain or generic trait, the trait of
/// `Fn(..)` sugar, or the bound under a `for<'a>` binder.
fn bound_trait(bound: Node<'_>, depth: usize) -> Option<NamedTarget<'_>> {
    if depth > MAX_TYPE_WRAPPING_DEPTH {
        return None;
    }
    match bound.kind() {
        "higher_ranked_trait_bound" => {
            bound_trait(bound.child_by_field_name("type")?, depth.saturating_add(1))
        }
        "function_type" => named_type(bound.child_by_field_name("trait")?, depth.saturating_add(1)),
        _ => named_type(bound, depth),
    }
}

/// The named type a type path denotes: `T`, `m::T` (leaf `T`), or the base of
/// `T<A>` / `m::T::<A>`. The path excludes type arguments.
fn named_type(node: Node<'_>, depth: usize) -> Option<NamedTarget<'_>> {
    if depth > MAX_TYPE_WRAPPING_DEPTH {
        return None;
    }
    match node.kind() {
        "type_identifier" => Some(NamedTarget::unqualified(node)),
        "scoped_type_identifier" | "scoped_identifier" => Some(NamedTarget {
            path: node,
            leaf: node.child_by_field_name("name")?,
        }),
        "generic_type" | "generic_type_with_turbofish" => {
            named_type(node.child_by_field_name("type")?, depth.saturating_add(1))
        }
        _ => None,
    }
}
