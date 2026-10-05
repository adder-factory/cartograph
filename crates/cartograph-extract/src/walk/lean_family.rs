//! Lean 4 structural extraction.
//!
//! Imports, namespaces, structures with fields, inductive types with their
//! constructors, definitions, theorems, and abbreviations. The root `module`
//! node is the file itself and is never a declaration.

use cartograph_domain::{SymbolKind, Visibility};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    ExtractionBuilder,
    family_support::{
        DeclarationShape, MAX_RETAINED_TEXT_BYTES, ScopeVisit, SymbolEmission, bounded_name,
        emit_declaration, emit_import_reference, literal_free_signature, visit_in_scope,
        with_owner,
    },
    sql_family::OwnerScopeInput,
    syntax::{has_child_kind, named_children},
};

/// Kind suffix of the parenthesized field binders that may group several names.
const GROUPED_BINDER_SUFFIX: &str = "_binder";

/// Dispatch one Lean node; `false` lets the walker descend.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "import" => visit_import(builder, node),
        "def" | "theorem" => visit_definition(builder, node, SymbolKind::Function),
        "abbrev" => visit_definition(builder, node, SymbolKind::TypeAlias),
        "structure" => visit_members(
            builder,
            MemberContainer {
                node,
                kind: SymbolKind::Struct,
                member_field: "fields",
                member_kind: SymbolKind::Field,
            },
        ),
        "inductive" | "class_inductive" => visit_members(
            builder,
            MemberContainer {
                node,
                kind: SymbolKind::Enum,
                member_field: "constructors",
                member_kind: SymbolKind::EnumMember,
            },
        ),
        "namespace" => visit_namespace(builder, node, depth),
        _ => Ok(false),
    }
}

/// An `import` names one module dependency.
fn visit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(module) = node.child_by_field_name("module") else {
        return Ok(false);
    };
    if let Some(name) = declared_name(builder, module)? {
        emit_import_reference(builder, module, name)?;
    }
    Ok(true)
}

/// Bounded text of a name node.
fn declared_name(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    bounded_name(builder.context.text(node))
        .map(|name| builder.context.copy_text(name))
        .transpose()
}

/// Bounded text of the node's `name` field.
fn field_name(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    match node.child_by_field_name("name") {
        Some(name) => declared_name(builder, name),
        None => Ok(None),
    }
}

/// Lean declarations are public unless the `private` modifier hides them.
fn declared_shape(kind: SymbolKind, private: bool) -> DeclarationShape {
    DeclarationShape {
        visibility: private.then_some(Visibility::Private),
        ..DeclarationShape::plain(kind, !private)
    }
}

/// Whether the enclosing `declaration` carries the `private` modifier.
fn is_private(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|parent| parent.kind() == "declaration" && has_child_kind(parent, "private"))
}

/// A `def`, `theorem`, or `abbrev` declaration.
fn visit_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: SymbolKind,
) -> Result<bool, ExtractError> {
    let Some(name) = field_name(builder, node)? else {
        return Ok(false);
    };
    let signature = if kind == SymbolKind::Function {
        definition_signature(builder, node)?
    } else {
        None
    };
    let private = is_private(node);
    emit_declaration(
        builder,
        SymbolEmission {
            node,
            name,
            body: Some(node),
            signature,
            shape: declared_shape(kind, private),
        },
    )?;
    Ok(true)
}

/// `(binders) : Type` of a definition, when present and literal-free.
fn definition_signature(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let binders = named_children(node)
        .find(|child| child.kind() == "binders")
        .map(|binders| builder.context.text(binders).trim());
    let mut cursor = node.walk();
    let result_type = node
        .children_by_field_name("type", &mut cursor)
        .find(Node::is_named)
        .map(|result| builder.context.text(result).trim());
    let length = binders
        .map_or(0, str::len)
        .saturating_add(result_type.map_or(0, str::len));
    if length > MAX_RETAINED_TEXT_BYTES {
        return Ok(None);
    }
    let signature = match (binders, result_type) {
        (Some(binders), Some(result)) => format!("{binders} : {result}"),
        (None, Some(result)) => format!(": {result}"),
        (Some(binders), None) => binders.to_owned(),
        (None, None) => return Ok(None),
    };
    literal_free_signature(builder, &signature)
}

/// A structure or inductive type and how its members are found.
#[derive(Clone, Copy)]
struct MemberContainer<'tree> {
    node: Node<'tree>,
    kind: SymbolKind,
    member_field: &'static str,
    member_kind: SymbolKind,
}

/// Emit a structure or inductive type with its fields or constructors.
fn visit_members(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: MemberContainer<'_>,
) -> Result<bool, ExtractError> {
    let Some(name) = field_name(builder, input.node)? else {
        return Ok(false);
    };
    let private = is_private(input.node);
    let qualifier = builder.context.copy_text(&name)?;
    let owner = emit_declaration(
        builder,
        SymbolEmission {
            node: input.node,
            name,
            body: Some(input.node),
            signature: None,
            shape: declared_shape(input.kind, private),
        },
    )?;
    let mut cursor = input.node.walk();
    let members: Vec<Node<'_>> = input
        .node
        .children_by_field_name(input.member_field, &mut cursor)
        .collect();
    with_owner(
        builder,
        OwnerScopeInput {
            owner: &owner,
            kind: input.kind,
            name: &qualifier,
        },
        |builder| {
            for member in members {
                emit_member(builder, member, input.member_kind)?;
            }
            Ok(())
        },
    )?;
    Ok(true)
}

/// One structure field or inductive constructor; a grouped binder such as
/// `(x y : Nat)` declares one field per name.
fn emit_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    member: Node<'_>,
    kind: SymbolKind,
) -> Result<(), ExtractError> {
    for name_node in member_names(member) {
        builder.context.ensure_active()?;
        let Some(name) = declared_name(builder, name_node)? else {
            continue;
        };
        emit_declaration(
            builder,
            SymbolEmission {
                node: member,
                name,
                body: None,
                signature: None,
                shape: DeclarationShape::plain(kind, false),
            },
        )?;
    }
    Ok(())
}

/// The names a member declares: its `name` field, then, for a grouped binder,
/// the further identifiers the grammar parses into a `binders` node.
fn member_names(member: Node<'_>) -> Vec<Node<'_>> {
    let Some(first) = member.child_by_field_name("name") else {
        return Vec::new();
    };
    // Only a parenthesized field binder groups names; a constructor's own
    // `binders` are its parameters, not further members.
    if !member.kind().ends_with(GROUPED_BINDER_SUFFIX) {
        return vec![first];
    }
    let grouped = named_children(member)
        .filter(|child| child.kind() == "binders")
        .flat_map(named_children)
        .filter(|name| name.kind() == "identifier");
    std::iter::once(first).chain(grouped).collect()
}

/// A `namespace` block qualifies every declaration in its body.
fn visit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(name) = field_name(builder, node)? else {
        return Ok(false);
    };
    let qualifier = builder.context.copy_text(&name)?;
    let owner = emit_declaration(
        builder,
        SymbolEmission {
            node,
            name,
            body: None,
            signature: None,
            shape: DeclarationShape::plain(SymbolKind::Namespace, true),
        },
    )?;
    let mut cursor = node.walk();
    let body: Vec<Node<'_>> = node.children_by_field_name("body", &mut cursor).collect();
    visit_in_scope(
        builder,
        ScopeVisit {
            owner: &owner,
            kind: SymbolKind::Namespace,
            name: &qualifier,
            children: &body,
            depth,
        },
    )?;
    Ok(true)
}
