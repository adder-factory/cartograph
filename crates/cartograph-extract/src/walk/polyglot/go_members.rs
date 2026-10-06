//! Go package bindings, struct fields and embedding, interface embedding,
//! composite-literal construction, and cgo calls.
//!
//! - A package-level `var` or `const` (single, grouped, or `iota` spec) declares
//!   one variable or constant per name; `_` declares nothing. Its declared type
//!   is a `TypeOf` use by each name. When a spec lists one value per name
//!   (`var A, B = a(), b()`), each name owns its own value; otherwise (one
//!   value, or one call returning several) the first declared name owns the
//!   whole initializer. Inside a body a spec declares nothing, but its declared
//!   type is a `TypeOf` use by the enclosing function.
//! - Each named field of a named struct is a field symbol contained by the
//!   struct; the field's type is a `TypeOf` use by the struct, once per
//!   written type. An embedded field, or an embedded interface, extends its
//!   type instead.
//! - `T{..}`, `&pkg.T{..}`, and `T[A]{..}` instantiate `T`; slice, map, array,
//!   and channel literals construct no declared type. Each field key of such a
//!   literal (`Bio` in `Profile{Bio: b}`) is a `FieldAccess` by the enclosing
//!   owner, since the literal initialises that field, unless the file declares
//!   the type as a map, slice, or array type, whose keys are expressions.
//! - In a file that imports `"C"`, `C.name(..)` calls the C function `name`.
//!   The `C.name` path stays the lookup identity, so no Go declaration that
//!   merely shares the name is chosen.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, symbol_signature_is_search_safe};
use tree_sitter::{Node, TreeCursor};

use super::{
    OwnedBody, go_exported, go_named_types,
    import_index::{CGO_PACKAGE, local_imports},
    type_targets::{
        NamedTarget, capture_declared_types, capture_shared_declared_types, emit_leaf_reference,
    },
    visit_owned_body,
};
use crate::{
    ExtractError, SymbolExportFlags,
    walk::{
        ExtractionBuilder, JoinedSignature, MAX_SAFE_SIGNATURE_BYTES, PendingReference,
        PendingSymbol, joined_signature, references, safe_assignment_signature,
        syntax::{descendants_including_root, named_children},
    },
};

/// The blank identifier, which declares nothing.
const BLANK_IDENTIFIER: &str = "_";
/// Deepest pointer/generic wrapping unwrapped to reach a named type.
const MAX_TYPE_WRAPPING_DEPTH: usize = 8;

/// One `var`/`const` spec declared at package scope.
#[derive(Clone, Copy)]
struct PackageSpec<'tree> {
    declaration: Node<'tree>,
    spec: Node<'tree>,
    kind: SymbolKind,
    depth: usize,
}

/// One name of a package-scope spec.
#[derive(Clone, Copy)]
struct PackageBinding<'tree> {
    spec: PackageSpec<'tree>,
    name: Node<'tree>,
    /// The spec's initializer list, found once per spec.
    value: Option<Node<'tree>>,
    /// Whether this is the spec's only name, which then owns the whole spec.
    sole: bool,
}

/// One part of a package spec (a value or its declared type) and the binding
/// that owns it, with its symbol and qualifier (`None` for `_`).
struct SpecPart<'tree> {
    owner: Option<(SymbolId, String)>,
    node: Node<'tree>,
    depth: usize,
}

/// One named field of a named struct.
#[derive(Clone, Copy)]
struct StructField<'tree> {
    field: Node<'tree>,
    name: Node<'tree>,
    field_type: Node<'tree>,
    /// The name is the declaration's only one.
    sole: bool,
}

/// Declarations owned by this module. Returns whether `node` was consumed.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(false);
    }
    match node.kind() {
        "var_declaration" | "const_declaration" if builder.owners.is_empty() => {
            visit_package_bindings(builder, node, depth)?;
            Ok(true)
        }
        "var_spec" | "const_spec" => {
            capture_local_spec_type(builder, node)?;
            Ok(false)
        }
        "field_declaration" => {
            visit_struct_field(builder, node)?;
            Ok(false)
        }
        "type_elem" => {
            visit_interface_embed(builder, node)?;
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Record `C.name(..)` in a cgo file as a call of the C function `name`.
/// Returns whether the call was consumed.
pub(super) fn capture_cgo_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
) -> Result<bool, ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(false);
    }
    let Some(function) = call
        .child_by_field_name("function")
        .filter(|function| function.kind() == "selector_expression")
    else {
        return Ok(false);
    };
    let (Some(operand), Some(field)) = (
        function.child_by_field_name("operand"),
        function.child_by_field_name("field"),
    ) else {
        return Ok(false);
    };
    if operand.kind() != "identifier"
        || builder.context.text(operand).trim() != CGO_PACKAGE
        || !imports_cgo(builder)?
    {
        return Ok(false);
    }
    emit_leaf_reference(
        builder,
        builder.owners.last().cloned(),
        NamedTarget {
            path: function,
            leaf: field,
        }
        .reference(ReferenceKind::Calls),
    )?;
    Ok(true)
}

/// Record a composite literal of a named type as instantiating that type.
pub(super) fn capture_composite_literal(
    builder: &mut ExtractionBuilder<'_, '_>,
    literal: Node<'_>,
) -> Result<(), ExtractError> {
    if !builder.optional_facts.admit() {
        return Ok(());
    }
    let Some(literal_type) = literal.child_by_field_name("type").filter(|node| {
        matches!(
            node.kind(),
            "type_identifier" | "qualified_type" | "generic_type"
        )
    }) else {
        return Ok(());
    };
    let Some(target) = named_type(literal_type, 0) else {
        return Ok(());
    };
    emit_leaf_reference(
        builder,
        builder.owners.last().cloned(),
        target.reference(ReferenceKind::Instantiates),
    )?;
    if go_named_types::names_container_type(builder, literal_type)? {
        return Ok(());
    }
    capture_literal_field_keys(builder, literal)
}

/// Record each field key of a named-type literal (`Bio` in `Profile{Bio: b}`)
/// as a `FieldAccess` by the enclosing owner: the literal initialises that
/// field, as `p.Bio = b` would assign it. Positional elements and keys that
/// are not plain names (`{f(): v}`) name no field, and a literal of a map,
/// slice, or array type the file declares has expression keys instead.
fn capture_literal_field_keys(
    builder: &mut ExtractionBuilder<'_, '_>,
    literal: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(body) = literal.child_by_field_name("body") else {
        return Ok(());
    };
    for element in named_children(body).filter(|element| element.kind() == "keyed_element") {
        builder.context.ensure_active()?;
        let Some(key) = element
            .child_by_field_name("key")
            .and_then(|key| named_children(key).find(|name| !name.is_extra()))
            .filter(|name| name.kind() == "identifier")
        else {
            continue;
        };
        let name = builder.context.owned_text(key)?;
        references::push_reference(
            builder,
            PendingReference {
                owner: builder.owners.last().cloned(),
                name,
                kind: ReferenceKind::FieldAccess,
                node: key,
            },
        )?;
    }
    Ok(())
}

/// Whether the file has the cgo `import "C"`, binding `C` to the cgo
/// pseudo-package rather than to an aliased package.
fn imports_cgo(builder: &mut ExtractionBuilder<'_, '_>) -> Result<bool, ExtractError> {
    Ok(local_imports(builder, CGO_PACKAGE)?.is_some_and(|imports| imports.cgo))
}

/// Whether the file binds an import under `local_name` (a package name, so an
/// identifier spelled that way is a package qualifier, not a value).
pub(super) fn imports_package(
    builder: &mut ExtractionBuilder<'_, '_>,
    local_name: &str,
) -> Result<bool, ExtractError> {
    Ok(local_imports(builder, local_name)?.is_some())
}

fn visit_package_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let kind = if declaration.kind() == "const_declaration" {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    for child in named_children(declaration) {
        let input = PackageSpec {
            declaration,
            spec: child,
            kind,
            depth,
        };
        match child.kind() {
            "var_spec" | "const_spec" => visit_package_spec(builder, input)?,
            "var_spec_list" => visit_package_spec_list(builder, input)?,
            _ => {}
        }
    }
    Ok(())
}

/// The specs of a grouped `var ( .. )` block, which the grammar wraps in a list.
fn visit_package_spec_list(
    builder: &mut ExtractionBuilder<'_, '_>,
    list: PackageSpec<'_>,
) -> Result<(), ExtractError> {
    for spec in named_children(list.spec).filter(|spec| spec.kind() == "var_spec") {
        visit_package_spec(builder, PackageSpec { spec, ..list })?;
    }
    Ok(())
}

fn visit_package_spec(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PackageSpec<'_>,
) -> Result<(), ExtractError> {
    let spec = input.spec;
    let value = spec.child_by_field_name("value");
    let declared_type = spec.child_by_field_name("type");
    let mut cursor = spec.walk();
    let names = declared_names(&spec, &mut cursor).count();
    let paired = names > 1 && value.is_some_and(|value| initializers(value).count() == names);
    let mut values = value.filter(|_| paired).map(initializers);
    let mut first_owner = None;
    let mut typed_owners = Vec::new();
    let mut cursor = spec.walk();
    for name in declared_names(&spec, &mut cursor) {
        let owner = emit_package_binding(
            builder,
            PackageBinding {
                spec: input,
                name,
                value,
                sole: names == 1,
            },
        )?;
        if let Some((id, _)) = owner.as_ref().filter(|_| declared_type.is_some()) {
            typed_owners
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            typed_owners.push(id.clone());
        }
        if first_owner.is_none() {
            first_owner.clone_from(&owner);
        }
        if let Some(own_value) = values.as_mut().and_then(Iterator::next) {
            visit_owned_by(
                builder,
                SpecPart {
                    owner,
                    node: own_value,
                    depth: input.depth,
                },
            )?;
        }
    }
    if let Some(declared_type) = declared_type {
        // One traversal of the shared type serves every name (`var A, B T`).
        capture_shared_declared_types(builder, declared_type, &typed_owners)?;
        // The declared type can read constants too (`[Size]byte`).
        visit_owned_by(
            builder,
            SpecPart {
                owner: first_owner.clone(),
                node: declared_type,
                depth: input.depth,
            },
        )?;
    }
    match value.filter(|_| !paired) {
        Some(value) => visit_owned_by(
            builder,
            SpecPart {
                owner: first_owner,
                node: value,
                depth: input.depth,
            },
        ),
        None => Ok(()),
    }
}

/// The names a declaration lists in its `name` field, without the commas
/// the grammar files under the same field (`const A, B = ..`).
fn declared_names<'cursor, 'tree>(
    declaration: &'cursor Node<'tree>,
    cursor: &'cursor mut TreeCursor<'tree>,
) -> impl Iterator<Item = Node<'tree>> + 'cursor {
    declaration
        .children_by_field_name("name", cursor)
        .filter(Node::is_named)
}

/// The initializer expressions of a spec's value list, without comments.
fn initializers(value: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    named_children(value).filter(|expression| !expression.is_extra())
}

/// Declare one name of a package spec, returning its symbol and qualifier
/// (or `None` for `_`).
fn emit_package_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PackageBinding<'_>,
) -> Result<Option<(SymbolId, String)>, ExtractError> {
    let PackageBinding {
        spec,
        name,
        value,
        sole,
    } = input;
    let text = builder.context.owned_text(name)?;
    if text == BLANK_IDENTIFIER {
        return Ok(None);
    }
    let signature = match value {
        Some(value) if sole => safe_assignment_signature(builder, value)?,
        _ => None,
    };
    // A sole name spans its spec; names sharing a spec span only themselves,
    // so a long multi-name spec is not re-analyzed once per name.
    let declaration = if sole { spec.spec } else { name };
    let qualifier = builder.context.copy_text(&text)?;
    let id = builder.emit_symbol(PendingSymbol {
        kind: spec.kind,
        name: text,
        span_node: declaration,
        structural_node: declaration,
        doc_anchor: if spec.spec.parent().map(|parent| parent.id()) == Some(spec.declaration.id()) {
            spec.declaration
        } else {
            spec.spec
        },
        body_node: value.filter(|_| sole),
        declaration_only: false,
        signature,
        export: SymbolExportFlags::new(go_exported(builder, name), false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    Ok(Some((id, qualifier)))
}

/// Visit part of a spec owned by its binding, or by the enclosing scope when
/// the binding is `_`.
fn visit_owned_by(
    builder: &mut ExtractionBuilder<'_, '_>,
    part: SpecPart<'_>,
) -> Result<(), ExtractError> {
    match part.owner {
        Some((owner, qualifier)) => visit_owned_body(
            builder,
            OwnedBody {
                owner,
                qualifier,
                body: part.node,
                depth: part.depth,
            },
        ),
        None => builder.visit(part.node, part.depth.saturating_add(1)),
    }
}

/// A spec inside a body declares no symbol, but its type is still used by the
/// enclosing function.
fn capture_local_spec_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    spec: Node<'_>,
) -> Result<(), ExtractError> {
    let (Some(owner), Some(declared_type)) = (
        builder.owners.last().cloned(),
        spec.child_by_field_name("type"),
    ) else {
        return Ok(());
    };
    capture_declared_types(builder, declared_type, &owner)
}

/// Declare the fields of a named struct's field declaration. The usage walk
/// still visits the declaration, for values its type reads (`[Size]byte`).
fn visit_struct_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    field: Node<'_>,
) -> Result<(), ExtractError> {
    if !in_named_struct(field) {
        return Ok(());
    }
    let (Some(owner), Some(field_type)) = (
        builder.owners.last().cloned(),
        field.child_by_field_name("type"),
    ) else {
        return Ok(());
    };
    let mut cursor = field.walk();
    let names = declared_names(&field, &mut cursor).count();
    let mut cursor = field.walk();
    for name in declared_names(&field, &mut cursor) {
        emit_struct_field(
            builder,
            StructField {
                field,
                name,
                field_type,
                sole: names == 1,
            },
        )?;
    }
    if names > 0 {
        capture_declared_types(builder, field_type, &owner)?;
    } else if let Some(target) = named_type(field_type, 0) {
        emit_leaf_reference(
            builder,
            Some(owner),
            target.reference(ReferenceKind::Extends),
        )?;
    }
    Ok(())
}

fn emit_struct_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: StructField<'_>,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(input.name)?;
    let type_text = builder.context.text(input.field_type).trim();
    let signature = if name.len().saturating_add(type_text.len()) < MAX_SAFE_SIGNATURE_BYTES
        && !contains_comment(input.field_type)
    {
        Some(joined_signature(
            builder,
            JoinedSignature::words(&name, type_text),
        )?)
        .filter(|signature| symbol_signature_is_search_safe(SymbolKind::Field, signature))
    } else {
        None
    };
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Field,
        name,
        span_node: input.name,
        // Names sharing a declaration (`A, B int`) are analyzed as themselves,
        // so a long name list is not re-analyzed once per name.
        structural_node: if input.sole { input.field } else { input.name },
        doc_anchor: input.field,
        body_node: None,
        declaration_only: false,
        signature,
        export: SymbolExportFlags::new(go_exported(builder, input.name), false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    Ok(())
}

/// Whether a (signature-bounded) type expression holds a comment, whose text
/// a signature must never copy.
fn contains_comment(type_expression: Node<'_>) -> bool {
    descendants_including_root(type_expression).any(|node| node.kind() == "comment")
}

/// Record an interface's embedded interface as one it extends. The usage walk
/// still visits the element, for values a constraint reads (`~[len(s)]byte`).
fn visit_interface_embed(
    builder: &mut ExtractionBuilder<'_, '_>,
    element: Node<'_>,
) -> Result<(), ExtractError> {
    let named_interface = element
        .parent()
        .filter(|parent| parent.kind() == "interface_type")
        .and_then(|interface| interface.parent())
        .is_some_and(|spec| spec.kind() == "type_spec");
    if !named_interface {
        return Ok(());
    }
    let Some(owner) = builder.owners.last().cloned() else {
        return Ok(());
    };
    let mut types = named_children(element);
    let (Some(embedded), None) = (types.next(), types.next()) else {
        return Ok(());
    };
    if let Some(target) = named_type(embedded, 0) {
        emit_leaf_reference(
            builder,
            Some(owner),
            target.reference(ReferenceKind::Extends),
        )?;
    }
    Ok(())
}

/// Whether a field sits directly in a struct that a `type` spec names
/// (`type T struct { .. }`), not in an anonymous struct type.
fn in_named_struct(field: Node<'_>) -> bool {
    field
        .parent()
        .filter(|list| list.kind() == "field_declaration_list")
        .and_then(|list| list.parent())
        .filter(|structure| structure.kind() == "struct_type")
        .and_then(|structure| structure.parent())
        .is_some_and(|spec| spec.kind() == "type_spec")
}

/// The named type a type expression denotes: `T`, `pkg.T` (leaf `T`), or the
/// base of `T[A]` / `*T`. The path excludes type arguments.
fn named_type(node: Node<'_>, depth: usize) -> Option<NamedTarget<'_>> {
    if depth > MAX_TYPE_WRAPPING_DEPTH {
        return None;
    }
    match node.kind() {
        "type_identifier" => Some(NamedTarget::unqualified(node)),
        "qualified_type" => Some(NamedTarget {
            path: node,
            leaf: node.child_by_field_name("name")?,
        }),
        "generic_type" => named_type(node.child_by_field_name("type")?, depth.saturating_add(1)),
        "pointer_type" => named_type(named_children(node).next()?, depth.saturating_add(1)),
        _ => None,
    }
}
