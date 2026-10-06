//! Rust constant reads inside bodies and initializers.
//!
//! A constant-shaped identifier (`LIMIT`, `MAX_ROWS`; Rust's naming lints
//! reserve that shape for constants and statics) read by an owner references
//! the constant or static it names. Not reads: callees (already calls), names
//! bound anywhere in an irrefutable `let`, `for`, parameter, or closure
//! pattern or explicitly by `ref`/`mut`/`name @` in any pattern, declared item
//! names, assignment targets, labels and lifetimes, path segments (the whole
//! path is already a reference), macro and attribute tokens (the macro scanner
//! owns those), and names bound as a parameter or const generic parameter of
//! an enclosing function, closure, or item visible at the read (a nested item
//! sees no outer function's names). A constant-shaped name in a
//! refutable `match`/`if let` pattern is a constant pattern and so is a read.
//! Whenever a bound is exhausted (a deeper pattern, a larger parameter list)
//! the identifier is treated as bound, never as a read.

use cartograph_domain::ReferenceKind;
use tree_sitter::Node;

use super::parameter_bindings::{ScopeNameSet, ScopeQuery, node_text, scope_binds};
use crate::{
    ExtractError,
    walk::{
        ExtractionBuilder, PendingReference, references, rust_macro,
        syntax::{
            descendants_including_root, is_call_or_construction_target, is_rust_turbofish_callee,
            named_children,
        },
    },
};

/// Parents whose every identifier child is a token, label, binding, or path segment.
const NON_READ_PARENTS: [&str; 16] = [
    "token_tree",
    "token_tree_pattern",
    "attribute",
    "macro_invocation",
    "macro_definition",
    "scoped_identifier",
    "scoped_type_identifier",
    "use_list",
    "scoped_use_list",
    "use_as_clause",
    "use_declaration",
    "closure_parameters",
    "parameter",
    "const_parameter",
    "label",
    "lifetime",
];
/// Declarations whose binding field holds the declared name, not a read.
const BINDING_FIELDS: [(&str, &str); 5] = [
    ("const_item", "name"),
    ("static_item", "name"),
    ("function_item", "name"),
    ("mod_item", "name"),
    ("enum_variant", "name"),
];
/// Irrefutable binding sites, whose pattern field binds every name it holds.
const BINDING_PATTERN_SITES: [(&str, &str); 3] = [
    ("let_declaration", "pattern"),
    ("for_expression", "pattern"),
    ("parameter", "pattern"),
];
/// Patterns that nest further patterns.
const NESTED_PATTERN_KINDS: [&str; 10] = [
    "tuple_pattern",
    "tuple_struct_pattern",
    "struct_pattern",
    "field_pattern",
    "slice_pattern",
    "ref_pattern",
    "mut_pattern",
    "reference_pattern",
    "captured_pattern",
    "or_pattern",
];
/// Deepest pattern nesting followed to its binding site.
const MAX_PATTERN_DEPTH: usize = 32;
/// Pattern nodes that bind a name (`x`, and `S { x }` shorthand).
const PATTERN_BINDING_KINDS: [&str; 2] = ["identifier", "shorthand_field_identifier"];
/// Patterns whose identifier child is always an explicit binding.
const EXPLICIT_BINDING_PATTERNS: [&str; 2] = ["ref_pattern", "mut_pattern"];
/// Expressions whose `left` operand is written, not read.
const WRITTEN_LEFT_PARENTS: [&str; 2] = ["assignment_expression", "compound_assignment_expr"];
/// Scopes whose parameters or const generic parameters can shadow a constant.
const PARAMETER_SCOPES: [&str; 6] = [
    "function_item",
    "function_signature_item",
    "closure_expression",
    "impl_item",
    "trait_item",
    "struct_item",
];

/// Items whose body cannot see the parameters or generics of an enclosing
/// function or item: the scope walk stops at the first one, after its own
/// names and, for an associated item, those of its `impl` or `trait`.
const ITEM_BOUNDARIES: [&str; 12] = [
    "function_item",
    "function_signature_item",
    "impl_item",
    "trait_item",
    "struct_item",
    "enum_item",
    "union_item",
    "mod_item",
    "const_item",
    "static_item",
    "type_item",
    "foreign_mod_item",
];
/// Items whose generics an associated item in their `declaration_list` sees.
const ASSOCIATED_ITEM_OWNERS: [&str; 2] = ["impl_item", "trait_item"];

/// Record a constant-shaped identifier read as a reference to the constant.
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
    if !rust_macro::constant_shaped(name)
        || !is_read_position(node)
        || bound_by_enclosing_scope(builder, node, name)?
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

/// Whether an identifier is read here rather than called, bound, written, or
/// held as a token, label, or path segment.
fn is_read_position(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let kind = parent.kind();
    !(is_call_or_construction_target(node)
        || is_rust_turbofish_callee(node)
        || NON_READ_PARENTS.contains(&kind)
        || BINDING_FIELDS.iter().any(|(declaration, field)| {
            kind == *declaration && is_field_child(parent, field, node)
        })
        || (WRITTEN_LEFT_PARENTS.contains(&kind) && is_field_child(parent, "left", node))
        || in_binding_pattern(node))
}

/// Whether an identifier is bound by a pattern: explicitly (`ref X`,
/// `mut X`, `X @ ..`) anywhere, or by an irrefutable pattern (`let (A, b)`,
/// `|(X,)|`, `fn f(&Y: &u8)`) however deeply it nests.
fn in_binding_pattern(node: Node<'_>) -> bool {
    if node.parent().is_some_and(|parent| {
        EXPLICIT_BINDING_PATTERNS.contains(&parent.kind())
            || (parent.kind() == "captured_pattern"
                && named_children(parent)
                    .next()
                    .is_some_and(|first| first.id() == node.id()))
    }) {
        return true;
    }
    let mut child = node;
    for _ in 0..MAX_PATTERN_DEPTH {
        let Some(parent) = child.parent() else {
            return false;
        };
        if NESTED_PATTERN_KINDS.contains(&parent.kind()) {
            child = parent;
            continue;
        }
        return parent.kind() == "closure_parameters"
            || BINDING_PATTERN_SITES.iter().any(|(site, field)| {
                parent.kind() == *site && is_field_child(parent, field, child)
            });
    }
    true
}

/// Whether `name` is a parameter or const generic parameter of a scope
/// enclosing `node` that is visible there: closures see their enclosing
/// function's names, but a nested item (`fn`, `mod`, `const`, ...) sees none
/// of the scopes outside it beyond its own `impl` or `trait` generics, so the
/// walk stops at the first item boundary. Each scope's names are collected
/// once per file. The macro scanner asks the same question for a
/// constant-shaped token in a macro's expression arguments, so such a token
/// is read exactly when direct code would read it.
pub(in crate::walk) fn bound_by_enclosing_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    let mut ancestor = node.parent();
    for _ in 0..=builder.maximum_ast_depth {
        let Some(current) = ancestor else {
            return Ok(false);
        };
        if scope_binds_name(builder, current, name)? {
            return Ok(true);
        }
        if ITEM_BOUNDARIES.contains(&current.kind()) {
            return match associated_item_owner(current) {
                Some(owner) => scope_binds_name(builder, owner, name),
                None => Ok(false),
            };
        }
        ancestor = current.parent();
    }
    Ok(true)
}

/// Whether `scope` is a parameter scope that binds `name`.
fn scope_binds_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    if !PARAMETER_SCOPES.contains(&scope.kind()) {
        return Ok(false);
    }
    scope_binds(
        builder,
        ScopeQuery {
            scope,
            name,
            collect: collect_scope_names,
        },
    )
}

/// The `impl` or `trait` whose body directly holds `item`, if any.
fn associated_item_owner(item: Node<'_>) -> Option<Node<'_>> {
    item.parent()
        .filter(|body| body.kind() == "declaration_list")
        .and_then(|body| body.parent())
        .filter(|owner| ASSOCIATED_ITEM_OWNERS.contains(&owner.kind()))
}

/// Collect the constant-shaped names a scope's parameter patterns and const
/// generic parameters bind, until the scan budget saturates the scope.
fn collect_scope_names(
    source: &str,
    scope: Node<'_>,
    names: &mut ScopeNameSet,
) -> Result<(), ExtractError> {
    for parameter in scope
        .child_by_field_name("parameters")
        .into_iter()
        .flat_map(named_children)
    {
        if !names.scan() {
            return Ok(());
        }
        if !collect_parameter_pattern_names(source, parameter, names)? {
            return Ok(());
        }
    }
    for parameter in scope
        .child_by_field_name("type_parameters")
        .into_iter()
        .flat_map(named_children)
    {
        if !names.scan() {
            return Ok(());
        }
        if let Some(bound) = parameter
            .child_by_field_name("name")
            .filter(|_| parameter.kind() == "const_parameter")
        {
            bind_constant_shaped(source, bound, names)?;
        }
    }
    Ok(())
}

/// Collect one parameter pattern, returning `false` when its scan budget is spent.
fn collect_parameter_pattern_names(
    source: &str,
    parameter: Node<'_>,
    names: &mut ScopeNameSet,
) -> Result<bool, ExtractError> {
    let pattern = if parameter.kind() == "parameter" {
        parameter.child_by_field_name("pattern")
    } else {
        Some(parameter)
    };
    for candidate in pattern.into_iter().flat_map(descendants_including_root) {
        if !names.scan() {
            return Ok(false);
        }
        if PATTERN_BINDING_KINDS.contains(&candidate.kind()) {
            bind_constant_shaped(source, candidate, names)?;
        }
    }
    Ok(true)
}

/// Record a bound name when it could be mistaken for a constant read.
fn bind_constant_shaped(
    source: &str,
    bound: Node<'_>,
    names: &mut ScopeNameSet,
) -> Result<(), ExtractError> {
    let name = node_text(source, bound);
    if rust_macro::constant_shaped(name) {
        names.bind(name)?;
    }
    Ok(())
}

fn is_field_child(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|candidate| candidate.id() == child.id())
}
