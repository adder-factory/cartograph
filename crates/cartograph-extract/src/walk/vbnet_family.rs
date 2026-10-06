//! VB.NET structural extraction.
//!
//! Maps the VB grammar's block declarations onto typed symbols (classes,
//! interfaces, structures, enums, modules, namespaces), members (methods,
//! constructors, properties, per-declarator fields, constants, locals), `Imports`
//! statements, and invocation/construction references. Visibility comes from the
//! `modifiers` list (`Friend` is internal) and `Shared` marks static members.
//!
//! The grammar only accepts `Inherits`/`Implements` on the class line itself;
//! the idiomatic statement on the following line is recovered from its source
//! line (see [`heritage`]) instead of becoming a bogus field.

use cartograph_domain::{
    ReferenceKind, SymbolId, SymbolKind, Visibility, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, references,
    syntax::{named_children, span_for},
    with_root_scope,
};

mod heritage;
pub(super) use heritage::HeritageSeen;

const MAX_SIGNATURE_BYTES: usize = 512;
const MAX_REFERENCE_TARGET_BYTES: usize = 512;
const CONSTRUCTOR_NAME: &str = "New";
/// `End ` prefix of the line that closes a routine body.
const END_KEYWORD: &str = "end ";
const SELF_RECEIVER_PREFIX: &str = "Me.";

/// One VB.NET declaration emitted as a symbol.
struct VbMember<'tree> {
    /// Declaration carrying the modifiers and documentation.
    node: Node<'tree>,
    name: String,
    kind: SymbolKind,
    signature: Option<String>,
    body: Option<Node<'tree>>,
    /// The member's own range when one declaration declares several members.
    declarator: Option<Node<'tree>>,
}

/// A symbol whose children are visited with it as the current owner.
struct VbScope<'tree> {
    node: Node<'tree>,
    id: SymbolId,
    kind: SymbolKind,
    name: String,
    depth: usize,
}

/// An `Inherits`/`Implements` clause accepted by the grammar on the type line.
#[derive(Clone, Copy)]
struct HeritageClause<'tree, 'owner> {
    clause: Node<'tree>,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
}

#[derive(Clone, Copy, Default)]
struct VbModifiers {
    visibility: Option<Visibility>,
    shared: bool,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "imports_statement" => visit_import(builder, node)?,
        "namespace_block" => visit_namespace(builder, node, depth)?,
        "class_block" | "interface_block" | "structure_block" | "enum_block" | "module_block" => {
            visit_type_block(builder, node, depth)?;
        }
        "method_declaration" | "constructor_declaration" => visit_callable(builder, node, depth)?,
        "property_declaration" => visit_property(builder, node, depth)?,
        "field_declaration" => visit_fields(builder, node, depth)?,
        "const_declaration"
        | "dim_statement"
        | "enum_member"
        | "delegate_declaration"
        | "event_declaration" => visit_named_member(builder, node, depth)?,
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let (field, kind) = match node.kind() {
        "invocation" => ("target", ReferenceKind::Calls),
        "new_expression" => ("type", ReferenceKind::Instantiates),
        "ERROR" => return capture_recovered_heritage(builder, node),
        _ => return Ok(()),
    };
    let Some(mut target) = node.child_by_field_name(field) else {
        return Ok(());
    };
    let mut name = reference_target(builder, target)?;
    // `Factory().Run(x)`: the receiver is a call, so only the member is named
    // (as the managed family does); the inner call is captured on its own.
    if name.is_none()
        && target.kind() == "member_access"
        && let Some(member) = target.child_by_field_name("member")
    {
        target = member;
        name = reference_target(builder, member)?;
    }
    let Some(name) = name else {
        return Ok(());
    };
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind,
            node: target,
        },
    )
}

/// `Imports System, System.Text` imports every listed namespace.
fn visit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let namespaces = {
        let mut cursor = node.walk();
        node.children_by_field_name("namespace", &mut cursor)
            .collect::<Vec<_>>()
    };
    for namespace in namespaces {
        builder.context.ensure_active()?;
        emit_import(builder, namespace)?;
    }
    Ok(())
}

fn emit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    namespace: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(module) = reference_target(builder, namespace)? else {
        return Ok(());
    };
    with_root_scope(builder, |builder| {
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Import,
            name: module.clone(),
            span_node: namespace,
            structural_node: namespace,
            doc_anchor: namespace,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })
    })?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: module.clone(),
            kind: ReferenceKind::Imports,
            node: namespace,
        },
    )?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: module,
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: span_for(namespace)?,
    })
}

fn visit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(name) = reference_target(builder, name_node)? else {
        return builder.visit_named_children(node, depth);
    };
    let id = builder.emit_symbol(PendingSymbol::namespace(node, name.clone()))?;
    visit_scope(
        builder,
        VbScope {
            node,
            id,
            kind: SymbolKind::Namespace,
            name,
            depth,
        },
    )
}

fn visit_type_block(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let kind = match node.kind() {
        "class_block" => SymbolKind::Class,
        "interface_block" => SymbolKind::Interface,
        "structure_block" => SymbolKind::Struct,
        "enum_block" => SymbolKind::Enum,
        _ => SymbolKind::Module,
    };
    let name = builder.context.owned_text(name_node)?;
    let id = emit_member(
        builder,
        VbMember {
            node,
            name: name.clone(),
            kind,
            signature: None,
            body: None,
            declarator: None,
        },
    )?;
    for (field, reference) in [
        ("inherits", ReferenceKind::Extends),
        ("implements", ReferenceKind::Implements),
    ] {
        if let Some(clause) = node.child_by_field_name(field) {
            capture_clause_types(
                builder,
                HeritageClause {
                    clause,
                    owner: &id,
                    kind: reference,
                },
            )?;
        }
    }
    visit_scope(
        builder,
        VbScope {
            node,
            id,
            kind,
            name,
            depth,
        },
    )
}

fn capture_clause_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: HeritageClause<'_, '_>,
) -> Result<(), ExtractError> {
    let HeritageClause {
        clause,
        owner,
        kind,
    } = input;
    for type_node in named_children(clause).filter(|child| child.kind() == "type") {
        builder.context.ensure_active()?;
        if let Some(name) = reference_target(builder, type_node)? {
            references::push_reference(
                builder,
                PendingReference {
                    owner: Some(owner.clone()),
                    name,
                    kind,
                    node: type_node,
                },
            )?;
        }
    }
    Ok(())
}

fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let name = if node.kind() == "constructor_declaration" {
        CONSTRUCTOR_NAME.to_owned()
    } else {
        let Some(name_node) = node.child_by_field_name("name") else {
            return builder.visit_named_children(node, depth);
        };
        builder.context.owned_text(name_node)?
    };
    let signature = callable_signature(builder, node)?;
    let body = has_end_keyword(builder, node).then_some(node);
    let id = emit_member(
        builder,
        VbMember {
            node,
            name: name.clone(),
            kind: SymbolKind::Method,
            signature,
            body,
            declarator: None,
        },
    )?;
    visit_scope(
        builder,
        VbScope {
            node,
            id,
            kind: SymbolKind::Method,
            name,
            depth,
        },
    )
}

/// `Function F(x As T) As R` is `(x As T) As R`. When the grammar misparses a
/// trailing `Implements I.M`, the real return type sits in an `ERROR` child that
/// precedes the bogus `return_type`, so the first type after the parameters wins.
fn callable_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return Ok(None);
    };
    let return_type = named_children(node)
        .filter(|child| child.start_byte() >= parameters.end_byte())
        .find_map(|child| match child.kind() {
            "type" => Some(child),
            "ERROR" => named_children(child).find(|nested| nested.kind() == "type"),
            _ => None,
        });
    let parameter_text = builder.context.text(parameters).trim();
    let return_text = return_type.map(|node| builder.context.text(node).trim());
    joined_signature(builder, parameter_text, return_text)
}

fn joined_signature(
    builder: &ExtractionBuilder<'_, '_>,
    left: &str,
    as_type: Option<&str>,
) -> Result<Option<String>, ExtractError> {
    let length = left
        .len()
        .checked_add(as_type.map_or(0, |value| value.len().saturating_add(" As ".len())))
        .ok_or(ExtractError::OutputLimit)?;
    if length > MAX_SIGNATURE_BYTES {
        return Ok(None);
    }
    builder.context.budget.ensure_string_length(length)?;
    let mut signature = String::new();
    signature
        .try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    signature.push_str(left);
    if let Some(as_type) = as_type {
        if !signature.is_empty() {
            signature.push(' ');
        }
        signature.push_str("As ");
        signature.push_str(as_type);
    }
    Ok(
        (!signature.is_empty() && callable_signature_is_literal_free(&signature))
            .then_some(signature),
    )
}

/// Whether a routine has a body: its declaration closes with an `End Sub` /
/// `End Function` line (the grammar keeps those keywords as hidden tokens), which
/// interface members and `Declare` statements never have.
fn has_end_keyword(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    let last_line = builder
        .context
        .text(node)
        .trim_end()
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .trim_start();
    last_line
        .get(..END_KEYWORD.len())
        .is_some_and(|keyword| keyword.eq_ignore_ascii_case(END_KEYWORD))
}

fn visit_property(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let signature = typed_signature(builder, node, &name)?;
    let id = emit_member(
        builder,
        VbMember {
            node,
            name: name.clone(),
            kind: SymbolKind::Property,
            signature,
            body: None,
            declarator: None,
        },
    )?;
    visit_scope(
        builder,
        VbScope {
            node,
            id,
            kind: SymbolKind::Property,
            name,
            depth,
        },
    )
}

/// `Name As Type` for a declaration (or declarator) carrying an `As` clause.
fn typed_signature(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    name: &str,
) -> Result<Option<String>, ExtractError> {
    let type_text = named_children(node)
        .find(|child| child.kind() == "as_clause")
        .and_then(|clause| clause.child_by_field_name("type"))
        .map(|type_node| builder.context.text(type_node).trim());
    joined_signature(builder, name, type_text)
}

/// The `As` type shared by each name of a flat multi-name declaration
/// (`Dim a, b As Integer` types both names; VB applies the clause to every
/// preceding bare name). An initializer ends the group: in `Dim a = F(), b As T`
/// the clause belongs to `b` only. One backward pass over the children.
fn shared_name_types<'tree>(node: Node<'tree>, names: &[Node<'tree>]) -> Vec<Option<Node<'tree>>> {
    let children = named_children(node).collect::<Vec<_>>();
    let mut types = vec![None; names.len()];
    let mut pending = names.len();
    let mut shared = None;
    for child in children.into_iter().rev() {
        match child.kind() {
            "as_clause" => shared = child.child_by_field_name("type"),
            "expression" => shared = None,
            _ => {}
        }
        if pending > 0 && child.id() == names[pending - 1].id() {
            pending -= 1;
            types[pending] = shared;
        }
    }
    types
}

/// The `As` type of each field declarator, shared backward the same way:
/// `Public a, b As Integer` types `a` with the clause of `b`.
fn shared_declarator_types<'tree>(declarators: &[Node<'tree>]) -> Vec<Option<Node<'tree>>> {
    let mut types = vec![None; declarators.len()];
    let mut shared = None;
    for (index, declarator) in declarators.iter().enumerate().rev() {
        let own = named_children(*declarator)
            .find(|child| child.kind() == "as_clause")
            .and_then(|clause| clause.child_by_field_name("type"));
        if own.is_some() {
            shared = own;
        } else if named_children(*declarator).any(|child| child.kind() == "expression") {
            shared = None;
        }
        types[index] = shared;
    }
    types
}

fn visit_fields(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if let Some(line) = heritage::heritage_line(builder, node)? {
        return heritage::emit_heritage_line(builder, &line);
    }
    let declarators = named_children(node)
        .filter(|child| child.kind() == "variable_declarator")
        .collect::<Vec<_>>();
    let types = shared_declarator_types(&declarators);
    for (declarator, type_node) in declarators.into_iter().zip(types) {
        builder.context.ensure_active()?;
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        let name = builder.context.owned_text(name_node)?;
        let type_text = type_node.map(|type_node| builder.context.text(type_node).trim());
        let signature = joined_signature(builder, &name, type_text)?;
        emit_member(
            builder,
            VbMember {
                node,
                name,
                kind: SymbolKind::Field,
                signature,
                body: None,
                declarator: Some(declarator),
            },
        )?;
        for value in named_children(declarator).filter(|child| child.kind() == "expression") {
            builder.visit(value, depth.saturating_add(1))?;
        }
    }
    Ok(())
}

/// An `ERROR` node starting an `Inherits`/`Implements` line; its children are
/// still visited, and a recovered field on the same line is deduplicated.
fn capture_recovered_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match heritage::heritage_line(builder, node)? {
        Some(line) => heritage::emit_heritage_line(builder, &line),
        None => Ok(()),
    }
}

fn visit_named_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    // `Dim a, b As Integer` and `Const A = 1, B = 2` declare several names.
    let names = {
        let mut cursor = node.walk();
        node.children_by_field_name("name", &mut cursor)
            .collect::<Vec<_>>()
    };
    if names.is_empty() {
        return builder.visit_named_children(node, depth);
    }
    let kind = named_member_kind(builder, node.kind());
    let several = names.len() > 1;
    let types = shared_name_types(node, &names);
    for (name_node, type_node) in names.into_iter().zip(types) {
        builder.context.ensure_active()?;
        let name = builder.context.owned_text(name_node)?;
        let signature = if kind == SymbolKind::TypeAlias {
            callable_signature(builder, node)?
        } else {
            let type_text = type_node.map(|type_node| builder.context.text(type_node).trim());
            joined_signature(builder, &name, type_text)?
        };
        emit_member(
            builder,
            VbMember {
                node,
                name,
                kind,
                signature,
                body: None,
                // Several names in one statement each get their own span.
                declarator: several.then_some(name_node),
            },
        )?;
    }
    for value in named_children(node).filter(|child| child.kind() == "expression") {
        builder.visit(value, depth.saturating_add(1))?;
    }
    Ok(())
}

fn named_member_kind(builder: &ExtractionBuilder<'_, '_>, node_kind: &str) -> SymbolKind {
    match node_kind {
        "const_declaration" => SymbolKind::Constant,
        "enum_member" => SymbolKind::EnumMember,
        "delegate_declaration" => SymbolKind::TypeAlias,
        "event_declaration" => SymbolKind::Resource,
        _ if matches!(
            builder.native_owner_kinds.last(),
            Some(SymbolKind::Method | SymbolKind::Property)
        ) =>
        {
            SymbolKind::Variable
        }
        _ => SymbolKind::Field,
    }
}

fn emit_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    member: VbMember<'_>,
) -> Result<SymbolId, ExtractError> {
    let modifiers = vb_modifiers(builder, member.node)?;
    let enum_member = member.kind == SymbolKind::EnumMember;
    let visibility = if enum_member {
        Some(Visibility::Public)
    } else {
        modifiers.visibility
    };
    let span = member.declarator.unwrap_or(member.node);
    builder.emit_symbol(PendingSymbol {
        kind: member.kind,
        name: member.name,
        span_node: span,
        structural_node: span,
        doc_anchor: member.node,
        body_node: member.body,
        declaration_only: member.kind == SymbolKind::Method && member.body.is_none(),
        signature: member.signature,
        export: crate::SymbolExportFlags::new(visibility == Some(Visibility::Public), false),
        async_symbol: false,
        static_member: modifiers.shared || enum_member,
        visibility,
    })
}

fn vb_modifiers(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<VbModifiers, ExtractError> {
    let mut modifiers = VbModifiers::default();
    let Some(list) = node.child_by_field_name("modifiers") else {
        return Ok(modifiers);
    };
    let mut private = false;
    let mut protected = false;
    let mut friend = false;
    let mut public = false;
    for modifier in named_children(list) {
        builder.context.ensure_active()?;
        let token = builder.context.text(modifier).trim();
        private |= token.eq_ignore_ascii_case("private");
        protected |= token.eq_ignore_ascii_case("protected");
        friend |= token.eq_ignore_ascii_case("friend");
        public |= token.eq_ignore_ascii_case("public");
        modifiers.shared |= token.eq_ignore_ascii_case("shared");
    }
    modifiers.visibility = [
        (private, Visibility::Private),
        (protected, Visibility::Protected),
        (friend, Visibility::Internal),
        (public, Visibility::Public),
    ]
    .into_iter()
    .find_map(|(present, visibility)| present.then_some(visibility));
    Ok(modifiers)
}

fn visit_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: VbScope<'_>,
) -> Result<(), ExtractError> {
    builder.owners.push(scope.id);
    builder.native_owner_kinds.push(scope.kind);
    builder.qualifiers.push(scope.name);
    let result = builder.visit_named_children(scope.node, scope.depth);
    builder.qualifiers.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

/// A dotted identifier path such as `Console.WriteLine` or `System.Text`,
/// without `Me.` and without generic `(Of T)` arguments; `None` for literals and
/// any other expression shape.
fn reference_target(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let text = builder.context.text(node).trim();
    let head = text.split('(').next().unwrap_or_default().trim_end();
    // Only a generic argument list may follow; `Factory().Run` is not a name.
    if !heritage::is_generic_suffix(&text[head.len()..]) {
        return Ok(None);
    }
    // VB keywords are case-insensitive: `me.Run()` and `ME.Run()` are `Run`.
    let raw = head
        .get(..SELF_RECEIVER_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(SELF_RECEIVER_PREFIX))
        .map_or(head, |prefix| &head[prefix.len()..]);
    if !is_bounded_dotted_text(raw) || !is_identifier_path_text(raw) {
        return Ok(None);
    }
    builder.context.copy_text(raw).map(Some)
}

/// Non-empty, within the reference-target bound, and without a leading or
/// trailing `.`.
fn is_bounded_dotted_text(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_REFERENCE_TARGET_BYTES
        && !raw.starts_with('.')
        && !raw.ends_with('.')
}

/// Only identifier bytes and `.` separators, not starting with a digit.
fn is_identifier_path_text(raw: &str) -> bool {
    raw.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
        && !raw.as_bytes().first().is_some_and(u8::is_ascii_digit)
}
