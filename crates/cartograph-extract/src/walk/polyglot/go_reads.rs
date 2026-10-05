//! Go exported-name reads inside bodies and initializers.
//!
//! An exported-shaped (`PascalCase`) identifier read by an owner references
//! the package-level binding it names (`return MaxN`, `sort.Slice(s, Less)`).
//! Not reads: callees (already calls), declared names, written left-hand
//! sides, struct-literal field keys (a literal of a written named type records
//! them as field accesses; the keys of a literal of a map, slice, or array type
//! the file declares are reads), package qualifiers, and names bound as a parameter, result,
//! receiver, or type parameter of an enclosing function. A
//! function-local variable spelled like a package binding is not modeled and
//! is the known false-positive shape (idiomatic Go locals are lower-case).

use cartograph_domain::ReferenceKind;
use tree_sitter::Node;

use super::{
    go_members::imports_package,
    go_named_types,
    parameter_bindings::{ScopeNameSet, ScopeQuery, node_text, scope_binds},
};
use crate::{
    ExtractError,
    walk::{
        ExtractionBuilder, PendingReference, references,
        syntax::{is_call_or_construction_target, named_children},
    },
};

/// Declarations whose `name` field binds the identifier rather than reading
/// it. In the Go grammar an `identifier` child of these can only be in the
/// `name` field (types are `type_identifier`s and values sit in an
/// `expression_list`), so the parent kind alone identifies a binding without
/// rescanning a long name list for every name.
const BINDING_PARENTS: [&str; 9] = [
    "var_spec",
    "const_spec",
    "parameter_declaration",
    "variadic_parameter_declaration",
    "type_parameter_declaration",
    "field_declaration",
    "function_declaration",
    "method_declaration",
    "type_spec",
];
/// Statements whose `left` expression list is written, not read.
const WRITTEN_LEFT_PARENTS: [&str; 4] = [
    "short_var_declaration",
    "range_clause",
    "assignment_statement",
    "receive_statement",
];
/// Composite-literal types whose keys are expressions rather than field names.
const EXPRESSION_KEYED_TYPES: [&str; 4] = [
    "map_type",
    "slice_type",
    "array_type",
    "implicit_length_array_type",
];
/// Deepest chain of elided nested literals (`{{..}}`) followed to its written type.
const MAX_ELIDED_LITERAL_DEPTH: usize = 16;
/// Fields of a function or literal that declare parameter-like names.
const PARAMETER_FIELDS: [&str; 4] = ["receiver", "parameters", "result", "type_parameters"];

/// Record an exported-shaped identifier read as a reference to its binding.
pub(super) fn capture_constant_read(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(());
    }
    let Some(owner) = builder.owners.last().cloned() else {
        return Ok(());
    };
    let snapshot = builder.context.snapshot;
    let name = node_text(snapshot.source(), node);
    if !exported_shaped(name)
        || !(is_read_position(node) || is_container_literal_key(builder, node)?)
        || imports_package(builder, name)?
        || bound_by_enclosing_function(builder, node, name)?
    {
        return Ok(());
    }
    let name = builder.context.owned_text(node)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner),
            name,
            kind: ReferenceKind::References,
            node,
        },
    )
}

/// Go's exported-identifier shape: an ASCII uppercase start.
fn exported_shaped(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Whether an identifier is read here rather than called, bound, written, or
/// used as a struct-literal field key.
fn is_read_position(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    !(is_call_or_construction_target(node)
        || BINDING_PARENTS.contains(&parent.kind())
        || (parent.kind() == "expression_list" && is_written_list(parent))
        || (parent.kind() == "literal_element" && is_field_key(parent)))
}

fn is_written_list(list: Node<'_>) -> bool {
    list.parent().is_some_and(|statement| {
        (WRITTEN_LEFT_PARENTS.contains(&statement.kind())
            && is_field_child(statement, "left", list))
            || (statement.kind() == "type_switch_statement"
                && is_field_child(statement, "alias", list))
    })
}

/// Whether a literal element is the key of a keyed element that may name a
/// struct field. Map, slice, and array literal keys are expressions and so are
/// reads, including in elided nested literals whose type the enclosing literal
/// proves; keys of a named, pointer, or unknown element type may be field names.
fn is_field_key(element: Node<'_>) -> bool {
    let Some(keyed) = element
        .parent()
        .filter(|keyed| keyed.kind() == "keyed_element" && is_field_child(*keyed, "key", element))
    else {
        return false;
    };
    !keyed_literal_type(keyed)
        .is_some_and(|literal_type| EXPRESSION_KEYED_TYPES.contains(&literal_type.kind()))
}

/// The type a keyed element's literal builds, through an elided `&T{..}`.
fn keyed_literal_type(keyed: Node<'_>) -> Option<Node<'_>> {
    keyed
        .parent()
        .filter(|value| value.kind() == "literal_value")
        .and_then(|value| literal_value_type(value, 0))
        .and_then(pointee)
}

/// Whether an identifier the syntax leaves as a possible field key is the key
/// of a literal whose named type the file declares as a map, slice, or array
/// type (`M{Key: 1}` after `type M map[string]int`), and so an expression read.
fn is_container_literal_key(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(literal_type) = node
        .parent()
        .filter(|element| element.kind() == "literal_element" && is_field_key(*element))
        .and_then(|element| element.parent())
        .and_then(keyed_literal_type)
    else {
        return Ok(false);
    };
    go_named_types::names_container_type(builder, literal_type)
}

/// The type an elided `&T{..}` literal builds: `T` for a pointer type.
fn pointee(literal_type: Node<'_>) -> Option<Node<'_>> {
    if literal_type.kind() == "pointer_type" {
        named_children(literal_type).next()
    } else {
        Some(literal_type)
    }
}

/// The type of a composite literal's value: written on the literal, or for
/// an elided nested literal the key, value, or element type of the enclosing
/// literal's type.
fn literal_value_type(value: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > MAX_ELIDED_LITERAL_DEPTH {
        return None;
    }
    let parent = value.parent()?;
    match parent.kind() {
        "composite_literal" => parent.child_by_field_name("type"),
        "literal_element" => {
            let container = parent.parent()?;
            let (outer_value, key) = match container.kind() {
                "keyed_element" => (
                    container.parent()?,
                    is_field_child(container, "key", parent),
                ),
                "literal_value" => (container, false),
                _ => return None,
            };
            let outer_type = literal_value_type(outer_value, depth.saturating_add(1))?;
            let outer_type = pointee(outer_type)?;
            match (outer_type.kind(), key) {
                ("map_type", true) => outer_type.child_by_field_name("key"),
                ("map_type", false) => outer_type.child_by_field_name("value"),
                ("slice_type" | "array_type" | "implicit_length_array_type", false) => {
                    outer_type.child_by_field_name("element")
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether `name` is a parameter, result, receiver, or type parameter of a
/// function or function literal enclosing `node`. Each function's names are
/// collected once per file.
fn bound_by_enclosing_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    let mut ancestor = node.parent();
    for _ in 0..=builder.maximum_ast_depth {
        let Some(current) = ancestor else {
            return Ok(false);
        };
        let callable = matches!(
            current.kind(),
            "function_declaration" | "method_declaration" | "func_literal"
        );
        if callable
            && scope_binds(
                builder,
                ScopeQuery {
                    scope: current,
                    name,
                    collect: collect_scope_names,
                },
            )?
        {
            return Ok(true);
        }
        if matches!(
            current.kind(),
            "function_declaration" | "method_declaration"
        ) {
            return Ok(false);
        }
        ancestor = current.parent();
    }
    Ok(true)
}

/// Collect the exported-shaped names a function's parameters, results,
/// receiver, and type parameters bind, until the scan budget saturates it.
fn collect_scope_names(
    source: &str,
    callable: Node<'_>,
    names: &mut ScopeNameSet,
) -> Result<(), ExtractError> {
    for declaration in PARAMETER_FIELDS
        .iter()
        .filter_map(|field| callable.child_by_field_name(field))
        .flat_map(named_children)
    {
        if !names.scan() {
            return Ok(());
        }
        let mut cursor = declaration.walk();
        for bound in declaration.children_by_field_name("name", &mut cursor) {
            if !names.scan() {
                return Ok(());
            }
            let bound = node_text(source, bound);
            if exported_shaped(bound) {
                names.bind(bound)?;
            }
        }
    }
    Ok(())
}

fn is_field_child(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    let mut cursor = parent.walk();
    parent
        .children_by_field_name(field, &mut cursor)
        .any(|candidate| candidate.id() == child.id())
}
