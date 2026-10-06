//! Swift structural extraction.
//!
//! Swift spells structs, enums, classes, actors, and extensions with one
//! `class_declaration` node, so the declaration keyword selects the symbol kind.
//! Members get Swift's visibility (internal by default), stored properties are
//! fields while computed properties stay accessor properties, enum cases are
//! members, and parameter, return, and field annotations become type
//! references. Declaration names drop Swift's identifier backticks, and blank
//! (recovered) names declare nothing. Imports, calls, and inheritance clauses
//! keep the shared generic capture, which already names them the way the
//! resolver expects; only a call of a backtick-escaped callee is named here.

mod types;

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, Visibility};
use tree_sitter::Node;

use crate::{ExtractError, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, generic_family,
    syntax::{children, named_children},
};
use types::{TypeCapture, TypeScope};

/// Owner kinds whose members are methods and fields rather than functions and globals.
const TYPE_OWNER_KINDS: [SymbolKind; 4] = [
    SymbolKind::Class,
    SymbolKind::Struct,
    SymbolKind::Enum,
    SymbolKind::Interface,
];
/// The `class_declaration` keyword that reopens an existing type.
const EXTENSION_KEYWORD: &str = "extension";
/// A call; its first named child is the callee.
const CALL_EXPRESSION: &str = "call_expression";
/// Optional-chaining and force-unwrap markers (`a?.b()`, `a!.b()`).
const UNWRAP_MARKERS: [char; 2] = ['?', '!'];

/// Where a declaration sits relative to its nearest owner.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    TopLevel,
    TypeMember,
    Local,
}

/// One declaration whose symbol owns the declaration's children.
struct OwnedDeclaration<'tree> {
    pending: PendingSymbol<'tree>,
    depth: usize,
}

/// A symbol visited as the owner of `nodes`, qualified by `qualifier`.
struct MemberScope<'tree, 'nodes> {
    nodes: &'nodes [Node<'tree>],
    id: SymbolId,
    kind: SymbolKind,
    qualifier: String,
    depth: usize,
    default_visibility: Option<Visibility>,
}

/// A type-level declaration about to be emitted.
struct TypeDeclaration<'tree> {
    node: Node<'tree>,
    kind: SymbolKind,
    name: String,
}

/// One `name` binding of a property declaration and the nodes after it up to
/// the next binding (its annotation, initializer, accessors, or observers).
struct Binding<'tree> {
    pattern: Node<'tree>,
    nodes: Vec<Node<'tree>>,
}

/// One binding of a property declaration being emitted.
#[derive(Clone, Copy)]
struct BindingVisit<'tree, 'binding> {
    declaration: Node<'tree>,
    binding: &'binding Binding<'tree>,
    placement: Placement,
    single: bool,
    depth: usize,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "source_file" => visit_source_file(builder, node, depth)?,
        "class_declaration" => visit_type(builder, node, depth)?,
        "protocol_declaration" => visit_protocol(builder, node, depth)?,
        "function_declaration" | "protocol_function_declaration" => {
            visit_function(builder, node, depth)?;
        }
        "property_declaration" | "protocol_property_declaration" => {
            return visit_property(builder, node, depth);
        }
        "enum_entry" => visit_enum_entry(builder, node)?,
        "typealias_declaration" | "associatedtype_declaration" => {
            visit_type_alias(builder, node)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() == CALL_EXPRESSION && capture_spelled_call(builder, node)? {
        return Ok(());
    }
    generic_family::capture_usage(builder, node)
}

/// A call whose callee spells an escaped identifier (`` value.`repeat`() ``)
/// or unwraps an optional on the way (`delegate?.update()`, `cache!.flush()`)
/// is named by its plain member path, as its declaration is; the shared
/// capture rejects backticks and unwrap markers and would drop the call.
/// Returns whether the callee needed this and so was handled here.
fn capture_spelled_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(callee) = named_children(node).next() else {
        return Ok(false);
    };
    let text = builder.context.text(callee);
    // A quoted callee is a string literal, whose content must never become
    // a name (the shared capture would strip the quotes and keep it), so it
    // names no call.
    if text.contains('"') {
        return Ok(true);
    }
    if !text.contains('`') && !text.contains(UNWRAP_MARKERS) {
        return Ok(false);
    }
    builder.context.budget.ensure_string_length(text.len())?;
    let mut plain = String::new();
    plain
        .try_reserve(text.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        let unwraps = UNWRAP_MARKERS.contains(&character)
            && characters.peek().is_none_or(|next| *next == '.');
        if character != '`' && !unwraps {
            plain.push(character);
        }
    }
    let Some(name) = generic_family::normalize_reference_name(&plain) else {
        return Ok(true);
    };
    super::references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::Calls,
            node: callee,
        },
    )?;
    Ok(true)
}

/// Extensions are file-scope only; visiting them after every other top-level
/// declaration, shallower extended paths first (`extension Outer` may declare
/// the `Inner` that `extension Outer.Inner` reopens), lets an extension reopen
/// a type declared later in the file.
fn visit_source_file(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let child_depth = depth.saturating_add(1);
    let mut extensions = Vec::new();
    for child in named_children(node) {
        if is_extension(child) {
            let path_length = child
                .child_by_field_name("name")
                .map_or(0, |name| types::type_path(name).len());
            extensions.push((path_length, child));
        } else {
            builder.visit(child, child_depth)?;
        }
    }
    extensions.sort_by_key(|(path_length, _)| *path_length);
    for (_, extension) in extensions {
        builder.visit(extension, child_depth)?;
    }
    Ok(())
}

fn is_extension(node: Node<'_>) -> bool {
    node.kind() == "class_declaration" && declaration_keyword(node) == EXTENSION_KEYWORD
}

fn declaration_keyword(node: Node<'_>) -> &str {
    node.child_by_field_name("declaration_kind")
        .map_or("class", |keyword| keyword.kind())
}

fn visit_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if is_extension(node) {
        return visit_extension(builder, node, depth);
    }
    let Some(name) = field_name(builder, node)? else {
        return builder.visit_named_children(node, depth);
    };
    let kind = match declaration_keyword(node) {
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        _ => SymbolKind::Class,
    };
    let pending = type_symbol(builder, TypeDeclaration { node, kind, name });
    visit_owned(builder, OwnedDeclaration { pending, depth })?;
    Ok(())
}

/// `extension Outer.Inner` adds members to the same-file `Outer::Inner`, or
/// declares a placeholder with that qualified identity: an extension of a
/// type declared elsewhere only augments it, so it never defines the type.
fn visit_extension(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let path = node
        .child_by_field_name("name")
        .map(types::type_path)
        .unwrap_or_default();
    let (Some((leaf, outer)), Some(qualified)) = (path.split_last(), joined_path(builder, &path)?)
    else {
        return builder.visit_named_children(node, depth);
    };
    if let Some((id, kind)) = reopened_type(builder, &qualified)? {
        let nodes = named_children(node).collect::<Vec<_>>();
        return visit_owned_children(
            builder,
            MemberScope {
                nodes: &nodes,
                id,
                kind,
                qualifier: qualified,
                depth,
                default_visibility: explicit_visibility(builder, node),
            },
        );
    }
    let (Some(name), Some(prefix)) = (declared_name(builder, *leaf)?, joined_path(builder, outer)?)
    else {
        return builder.visit_named_children(node, depth);
    };
    let mut pending = type_symbol(
        builder,
        TypeDeclaration {
            node,
            kind: SymbolKind::Class,
            name,
        },
    );
    pending.declaration_only = true;
    let prefixed = !prefix.is_empty();
    if prefixed {
        builder.qualifiers.push(prefix);
    }
    let result = visit_owned(builder, OwnedDeclaration { pending, depth });
    if prefixed {
        builder.qualifiers.pop();
    }
    result.map(|_| ())
}

/// `Outer.Inner` as the qualified-name path `Outer::Inner`, or `None` when a
/// segment is blank (a recovered MISSING identifier).
fn joined_path(
    builder: &ExtractionBuilder<'_, '_>,
    segments: &[Node<'_>],
) -> Result<Option<String>, ExtractError> {
    let mut joined = String::new();
    for segment in segments {
        let segment = unescaped(builder.context.text(*segment));
        if segment.is_empty() {
            return Ok(None);
        }
        if !joined.is_empty() {
            joined.push_str("::");
        }
        joined.push_str(segment);
    }
    builder.context.copy_text(&joined).map(Some)
}

/// The declaration's `name` field as a declared name (see [`declared_name`]).
fn field_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    match node.child_by_field_name("name") {
        Some(name) => declared_name(builder, name),
        None => Ok(None),
    }
}

/// A declared name without Swift's identifier escaping (`` `repeat` `` is
/// `repeat`, the name its uses carry), or `None` when the parser recovered the
/// name as a zero-width MISSING identifier: a nameless symbol would fail the
/// validation of the whole generation.
fn declared_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    builder.context.ensure_active()?;
    let name = unescaped(builder.context.text(node));
    if name.is_empty() || super::specifier_safety::specifier_may_carry_credential(name) {
        return Ok(None);
    }
    builder.context.copy_text(name).map(Some)
}

/// Identifier text without the backticks that let a keyword be a name.
fn unescaped(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed.strip_circumfix('`', '`').unwrap_or(trimmed).trim()
}

fn visit_protocol(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name) = field_name(builder, node)? else {
        return builder.visit_named_children(node, depth);
    };
    let pending = type_symbol(
        builder,
        TypeDeclaration {
            node,
            kind: SymbolKind::Interface,
            name,
        },
    );
    visit_owned(builder, OwnedDeclaration { pending, depth })?;
    Ok(())
}

fn type_symbol<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    declaration: TypeDeclaration<'tree>,
) -> PendingSymbol<'tree> {
    let TypeDeclaration { node, kind, name } = declaration;
    let visibility = swift_visibility(builder, node);
    PendingSymbol {
        kind,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: node.child_by_field_name("body"),
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::named(visibility == Visibility::Public),
        async_symbol: false,
        static_member: false,
        visibility: Some(visibility),
    }
}

/// The same-file type with this (relative) qualified name.
fn reopened_type(
    builder: &ExtractionBuilder<'_, '_>,
    name: &str,
) -> Result<Option<(SymbolId, SymbolKind)>, ExtractError> {
    let qualified_name = builder.qualified_name(name)?;
    Ok(builder
        .facts
        .symbols
        .iter()
        .find(|symbol| {
            TYPE_OWNER_KINDS.contains(&symbol.kind) && symbol.qualified_name == qualified_name
        })
        .map(|symbol| (symbol.id.clone(), symbol.kind)))
}

fn visit_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name) = field_name(builder, node)? else {
        return builder.visit_named_children(node, depth);
    };
    let requirement = node.kind() == "protocol_function_declaration";
    let kind = if requirement || placement(builder) == Placement::TypeMember {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let visibility = swift_visibility(builder, node);
    let body = node.child_by_field_name("body");
    let pending = PendingSymbol {
        kind,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only: body.is_none(),
        signature: None,
        export: SymbolExportFlags::named(visibility == Visibility::Public),
        async_symbol: has_token(node, "async"),
        static_member: is_static_member(node),
        visibility: Some(visibility),
    };
    let id = visit_owned(builder, OwnedDeclaration { pending, depth })?;
    let generics = types::generic_parameters(builder.context.source(), node);
    types::capture_callable_types(
        builder,
        node,
        TypeScope {
            owner: &id,
            generics: &generics,
        },
    )
}

/// Stored properties are fields (or top-level constants and variables),
/// computed properties are accessor properties, protocol requirements are
/// declaration-only properties, and function locals are not declarations.
/// `var a: A, b: B` declares one symbol per binding.
fn visit_property(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let placement = placement(builder);
    if placement == Placement::Local {
        return Ok(false);
    }
    let bindings = property_bindings(node);
    if bindings.is_empty() {
        return Ok(false);
    }
    let single = bindings.len() == 1;
    for binding in &bindings {
        visit_binding(
            builder,
            BindingVisit {
                declaration: node,
                binding,
                placement,
                single,
                depth,
            },
        )?;
    }
    Ok(true)
}

fn visit_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    visit: BindingVisit<'_, '_>,
) -> Result<(), ExtractError> {
    let BindingVisit {
        declaration,
        binding,
        placement,
        single,
        depth,
    } = visit;
    let name = match binding_name(binding.pattern) {
        Some(name_node) => declared_name(builder, name_node)?,
        None => None,
    };
    let Some(name) = name else {
        // A destructuring pattern (`let (a, b) = (f(), g())`) or a recovered
        // blank name declares no symbol, yet its initializer still makes calls.
        let child_depth = depth.saturating_add(1);
        return binding
            .nodes
            .iter()
            .try_for_each(|node| builder.visit(*node, child_depth));
    };
    let requirement = declaration.kind() == "protocol_property_declaration";
    let computed = binding_node(binding, "computed_property");
    let kind = if requirement || computed.is_some() {
        SymbolKind::Property
    } else if placement == Placement::TypeMember {
        SymbolKind::Field
    } else if binds_constant(declaration) {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    // Every binding spans its whole declaration statement, so an initializer
    // change is visible in its digest and later enrichment finds its owner.
    let visibility = swift_visibility(builder, declaration);
    let id = builder.emit_symbol(PendingSymbol {
        kind,
        name: name.clone(),
        span_node: declaration,
        structural_node: declaration,
        doc_anchor: declaration,
        body_node: computed,
        declaration_only: requirement,
        signature: None,
        export: SymbolExportFlags::named(visibility == Visibility::Public),
        async_symbol: false,
        static_member: is_static_member(declaration),
        visibility: Some(visibility),
    })?;
    let owned = if single {
        named_children(declaration).collect::<Vec<_>>()
    } else {
        binding.nodes.clone()
    };
    visit_owned_children(
        builder,
        MemberScope {
            nodes: &owned,
            id: id.clone(),
            kind,
            qualifier: name,
            depth,
            default_visibility: None,
        },
    )?;
    capture_binding_type(builder, visit, &id)
}

fn capture_binding_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    visit: BindingVisit<'_, '_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(annotation) = binding_node(visit.binding, "type_annotation") else {
        return Ok(());
    };
    let generics = types::generic_parameters(builder.context.source(), visit.declaration);
    types::capture_types(
        builder,
        TypeCapture::type_of(
            annotation,
            TypeScope {
                owner,
                generics: &generics,
            },
        ),
    )
}

fn binding_node<'tree>(binding: &Binding<'tree>, kind: &str) -> Option<Node<'tree>> {
    binding
        .nodes
        .iter()
        .copied()
        .find(|node| node.kind() == kind)
}

fn property_bindings(node: Node<'_>) -> Vec<Binding<'_>> {
    let mut bindings: Vec<Binding<'_>> = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return bindings;
    }
    loop {
        let child = cursor.node();
        if cursor.field_name() == Some("name") {
            bindings.push(Binding {
                pattern: child,
                nodes: Vec::new(),
            });
        } else if child.is_named()
            && let Some(binding) = bindings.last_mut()
        {
            binding.nodes.push(child);
        }
        if !cursor.goto_next_sibling() {
            return bindings;
        }
    }
}

/// `var x` binds `x`; protocol requirements wrap the binding pattern.
fn binding_name(pattern: Node<'_>) -> Option<Node<'_>> {
    if pattern.kind() == "simple_identifier" {
        return Some(pattern);
    }
    pattern
        .child_by_field_name("bound_identifier")
        .filter(|identifier| identifier.kind() == "simple_identifier")
}

fn binds_constant(node: Node<'_>) -> bool {
    named_children(node)
        .find(|child| child.kind() == "value_binding_pattern")
        .and_then(|binding| binding.child_by_field_name("mutability"))
        .is_some_and(|mutability| mutability.kind() == "let")
}

/// `case red, green` declares one member per name; raw values are never read.
fn visit_enum_entry(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let mut cursor = node.walk();
    let names = node
        .children_by_field_name("name", &mut cursor)
        .filter(|name| name.kind() == "simple_identifier")
        .collect::<Vec<_>>();
    for name_node in names {
        let Some(name) = declared_name(builder, name_node)? else {
            continue;
        };
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::EnumMember,
            name,
            span_node: name_node,
            structural_node: name_node,
            doc_anchor: node,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: SymbolExportFlags::named(false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })?;
    }
    Ok(())
}

fn visit_type_alias(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name) = field_name(builder, node)? else {
        return Ok(());
    };
    let pending = type_symbol(
        builder,
        TypeDeclaration {
            node,
            kind: SymbolKind::TypeAlias,
            name,
        },
    );
    builder.emit_symbol(pending)?;
    Ok(())
}

fn visit_owned(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: OwnedDeclaration<'_>,
) -> Result<SymbolId, ExtractError> {
    let node = declaration.pending.span_node;
    let kind = declaration.pending.kind;
    let qualifier = declaration.pending.name.clone();
    let default_visibility = if kind == SymbolKind::Interface || is_extension(node) {
        declaration.pending.visibility
    } else {
        None
    };
    let id = builder.emit_symbol(declaration.pending)?;
    let nodes = named_children(node).collect::<Vec<_>>();
    visit_owned_children(
        builder,
        MemberScope {
            nodes: &nodes,
            id: id.clone(),
            kind,
            qualifier,
            depth: declaration.depth,
            default_visibility,
        },
    )?;
    Ok(id)
}

fn visit_owned_children(
    builder: &mut ExtractionBuilder<'_, '_>,
    owner: MemberScope<'_, '_>,
) -> Result<(), ExtractError> {
    builder.owners.push(owner.id);
    builder.native_owner_kinds.push(owner.kind);
    builder.native_visibilities.push(owner.default_visibility);
    builder.qualifiers.push(owner.qualifier);
    let child_depth = owner.depth.saturating_add(1);
    let result = owner
        .nodes
        .iter()
        .try_for_each(|node| builder.visit(*node, child_depth));
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

fn placement(builder: &ExtractionBuilder<'_, '_>) -> Placement {
    match builder.native_owner_kinds.last() {
        None => Placement::TopLevel,
        Some(kind) if TYPE_OWNER_KINDS.contains(kind) => Placement::TypeMember,
        Some(_) => Placement::Local,
    }
}

/// `public`/`open` are public, `private`/`fileprivate` are private, everything
/// else uses the scope's member default, ordinarily `internal`. A setter-only
/// restriction such as `private(set)` leaves the getter's visibility intact.
fn swift_visibility(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Visibility {
    explicit_visibility(builder, node)
        .or_else(|| builder.native_visibilities.last().copied().flatten())
        .unwrap_or(Visibility::Internal)
}

fn explicit_visibility(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<Visibility> {
    let modifiers = named_children(node).find(|child| child.kind() == "modifiers")?;
    for modifier in named_children(modifiers).filter(|child| child.kind() == "visibility_modifier")
    {
        if has_token(modifier, "set") {
            continue;
        }
        match builder.context.text(modifier).trim() {
            "public" | "open" => return Some(Visibility::Public),
            "private" | "fileprivate" => return Some(Visibility::Private),
            "internal" | "package" => return Some(Visibility::Internal),
            _ => {}
        }
    }
    None
}

/// `static` and `class` members belong to the type, not an instance.
fn is_static_member(node: Node<'_>) -> bool {
    has_token(node, "class")
        || has_token(node, "static")
        || named_children(node)
            .filter(|child| child.kind() == "modifiers")
            .flat_map(named_children)
            .filter(|modifier| modifier.kind() == "property_modifier")
            .any(|modifier| has_token(modifier, "static") || has_token(modifier, "class"))
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    children(node).any(|child| !child.is_named() && child.kind() == kind)
}
