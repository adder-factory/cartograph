//! Same-file Go named types whose composite-literal keys are expressions.
//!
//! A key in `M{Key: 1}` is a struct field name only when `M` is a struct. When
//! the file itself declares `M` at package level as a map, slice, or array type
//! (`type M map[string]int`, `type L = []T`, `type P (map[K]V)`, or a named or
//! instantiated type of such a type, followed through at most
//! [`MAX_NAMED_TYPE_CHAIN`] same-file names), the key is an expression, as it
//! is in a literal of a written `map[..]` type. A type from another file or
//! package is unknown and keeps the field-name reading, and so does a name the
//! file also declares inside a function, since that local type may shadow the
//! package-level one where the literal is written. The file is indexed once,
//! and only when a literal of a plain named type asks.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::{
    ExtractError,
    walk::{
        ExtractionBuilder,
        syntax::{descendants_including_root, named_children},
    },
};

/// Underlying type kinds whose literal keys are expressions.
const EXPRESSION_KEYED_TYPES: [&str; 4] = [
    "map_type",
    "slice_type",
    "array_type",
    "implicit_length_array_type",
];
/// Type specification kinds that declare a name.
const TYPE_SPEC_KINDS: [&str; 2] = ["type_spec", "type_alias"];
/// Longest chain of same-file named types (`type A B; type B map[..]..`) followed.
const MAX_NAMED_TYPE_CHAIN: usize = 8;
/// Deepest parenthesized or instantiated wrapping unwrapped to reach a type.
const MAX_TYPE_WRAPPING_DEPTH: usize = 8;
/// Visited nodes between cancellation checks while indexing local types.
const INDEX_CANCELLATION_INTERVAL: usize = 1_024;

/// The file's package-level container type names, computed on first use.
#[derive(Default)]
pub(super) struct GoContainerTypes {
    names: Option<BTreeSet<String>>,
}

/// What one package-level type declaration's underlying type is.
enum Underlying {
    /// A map, slice, or array type.
    Container,
    /// Another plain or instantiated named type of the file, followed by name.
    Named(String),
    /// A struct, interface, or any other type.
    Other,
}

/// Whether a composite literal's written type (`M`, `M[K]`) names a container
/// type the file declares at package level and nowhere else.
pub(super) fn names_container_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    literal_type: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(base) = type_name(literal_type) else {
        return Ok(false);
    };
    if builder.polyglot.go_container_types.names.is_none() {
        let names = container_names(builder, literal_type)?;
        builder.polyglot.go_container_types.names = Some(names);
    }
    let name = builder.context.text(base).trim();
    Ok(builder
        .polyglot
        .go_container_types
        .names
        .as_ref()
        .is_some_and(|names| names.contains(name)))
}

/// The plain name a type names through parentheses and type arguments
/// (`M`, `(M)`, `M[K, V]`), if any.
fn type_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    for _ in 0..MAX_TYPE_WRAPPING_DEPTH {
        match current.kind() {
            "type_identifier" => return Some(current),
            "generic_type" => current = current.child_by_field_name("type")?,
            "parenthesized_type" => current = named_children(current).find(|n| !n.is_extra())?,
            _ => return None,
        }
    }
    None
}

/// Index the type declarations of the file containing `node` and return the
/// package-level names whose underlying type is a container and which no
/// function-local declaration reuses.
fn container_names(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<BTreeSet<String>, ExtractError> {
    let mut root = node;
    for _ in 0..=builder.maximum_ast_depth {
        let Some(parent) = root.parent() else {
            break;
        };
        root = parent;
    }
    let mut declared = BTreeMap::new();
    let mut local = BTreeSet::new();
    for (visited, declaration) in descendants_including_root(root).enumerate() {
        if visited.is_multiple_of(INDEX_CANCELLATION_INTERVAL) {
            builder.context.ensure_active()?;
        }
        if declaration.kind() != "type_declaration" {
            continue;
        }
        let package_level = declaration
            .parent()
            .is_some_and(|parent| parent.id() == root.id());
        for spec in
            named_children(declaration).filter(|spec| TYPE_SPEC_KINDS.contains(&spec.kind()))
        {
            let Some(name) = spec.child_by_field_name("name") else {
                continue;
            };
            let name = builder.context.owned_text(name)?;
            if !package_level {
                local.insert(name);
                continue;
            }
            let underlying = spec
                .child_by_field_name("type")
                .map_or(Ok(Underlying::Other), |spec_type| {
                    underlying(builder, spec_type)
                })?;
            declared.insert(name, underlying);
        }
    }
    let mut containers = BTreeSet::new();
    for name in declared.keys() {
        if !local.contains(name) && is_container(&declared, name) {
            containers.insert(builder.context.copy_text(name)?);
        }
    }
    Ok(containers)
}

/// Classify one declared underlying type, through parentheses.
fn underlying(
    builder: &mut ExtractionBuilder<'_, '_>,
    spec_type: Node<'_>,
) -> Result<Underlying, ExtractError> {
    let mut current = spec_type;
    for _ in 0..MAX_TYPE_WRAPPING_DEPTH {
        if current.kind() != "parenthesized_type" {
            break;
        }
        let Some(inner) = named_children(current).find(|inner| !inner.is_extra()) else {
            return Ok(Underlying::Other);
        };
        current = inner;
    }
    if EXPRESSION_KEYED_TYPES.contains(&current.kind()) {
        return Ok(Underlying::Container);
    }
    match type_name(current) {
        Some(name) => Ok(Underlying::Named(builder.context.owned_text(name)?)),
        None => Ok(Underlying::Other),
    }
}

/// Whether `name` reaches a container through at most [`MAX_NAMED_TYPE_CHAIN`] names.
fn is_container(declared: &BTreeMap<String, Underlying>, name: &str) -> bool {
    let mut current = name;
    for _ in 0..MAX_NAMED_TYPE_CHAIN {
        match declared.get(current) {
            Some(Underlying::Container) => return true,
            Some(Underlying::Named(next)) => current = next,
            Some(Underlying::Other) | None => return false,
        }
    }
    false
}
