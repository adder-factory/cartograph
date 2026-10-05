//! Solidity contract-member extraction.
//!
//! Contracts, interfaces, structs, enums, events, inheritance, calls, and
//! imports (their `Imports` references, bindings, and credential-screened
//! `Import` declarations) stay with the generic structural walker. This family
//! adds what the generic walker cannot see: functions and modifiers as contract
//! methods with visibility and literal-free signatures, special callables, enum
//! values, struct members and state variables as fields, and pragma
//! declarations.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, Visibility};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedReference};

use super::{
    ExtractionBuilder,
    family_support::{
        DeclarationShape, MAX_RETAINED_TEXT_BYTES, ScopeVisit, ScopedEmission, SymbolEmission,
        bounded_name, emit_declaration, emit_import_symbol, emit_scoped_declaration,
        literal_free_signature, register_scope_key, source_text, visit_in_scope,
    },
    generic_family,
    syntax::{children, descendants, named_children, span_for},
};

/// Owner kinds whose callables are methods rather than free functions.
const METHOD_OWNER_KINDS: &[SymbolKind] = &[SymbolKind::Class, SymbolKind::Interface];
/// Declarations whose functions and modifiers are in scope in their bodies.
const CONTRACT_KINDS: &[&str] = &[
    "contract_declaration",
    "interface_declaration",
    "library_declaration",
];
/// Scope-map key prefix for the callables each contract declares.
const CONTRACT_MEMBER_SCOPE_PREFIX: &str = "solidity-member:";
/// Scope-map key prefix for the parameters and locals each callable binds.
const LOCAL_SCOPE_PREFIX: &str = "solidity-local:";

/// Dispatch one Solidity node, delegating unhandled shapes to the generic walker.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let handled = match node.kind() {
        "function_definition" | "modifier_definition" => {
            let source = builder.context.snapshot.source();
            let name = node
                .child_by_field_name("name")
                .and_then(|name| bounded_name(source_text(source, name)));
            visit_callable(builder, CallableVisit { node, name, depth })?
        }
        "constructor_definition" | "fallback_receive_definition" => {
            let name = special_callable_name(node);
            visit_callable(builder, CallableVisit { node, name, depth })?
        }
        "library_declaration" => visit_library(builder, node, depth)?,
        "enum_value" => visit_enum_value(builder, node)?,
        "struct_member" | "state_variable_declaration" => visit_field(builder, node, depth)?,
        "pragma_directive" => visit_pragma(builder, node)?,
        _ => false,
    };
    if handled {
        return Ok(true);
    }
    if CONTRACT_KINDS.contains(&node.kind()) {
        register_contract_callables(builder, node)?;
    }
    generic_family::visit_declaration(builder, node, depth)
}

/// Capture calls; a bare call to a callable of the enclosing contract resolves
/// to that contract's member, since contract members are in scope in its body.
/// A modifier invocation in a function header calls that modifier.
pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let callee = match node.kind() {
        "call_expression" => bare_callee(node),
        "modifier_invocation" if invokes_modifier(node) => named_children(node)
            .next()
            .filter(|name| name.kind() == "identifier"),
        _ => None,
    };
    let Some(callee) = callee else {
        return generic_family::capture_usage(builder, node);
    };
    let member = enclosing_contract_member(builder, callee)?;
    if member.is_none() && node.kind() == "call_expression" {
        return generic_family::capture_usage(builder, node);
    }
    let name = builder.context.copy_text(callee_text(builder, callee))?;
    builder.emit_reference(ExtractedReference {
        owner: builder.owners.last().cloned(),
        name,
        resolution_name: member,
        kind: ReferenceKind::Calls,
        span: span_for(callee)?,
    })
}

/// A function or modifier header invokes modifiers; a constructor header's
/// invocations instead pass arguments to base-contract constructors.
fn invokes_modifier(invocation: Node<'_>) -> bool {
    invocation.parent().is_some_and(|parent| {
        matches!(parent.kind(), "function_definition" | "modifier_definition")
    })
}

/// A library is a contract-like container; its functions are its members.
fn visit_library(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let source = builder.context.snapshot.source();
    let Some(name) = node
        .child_by_field_name("name")
        .and_then(|name| bounded_name(source_text(source, name)))
    else {
        return Ok(false);
    };
    register_contract_callables(builder, node)?;
    let name = builder.context.copy_text(name)?;
    let body: Vec<Node<'_>> = node.child_by_field_name("body").into_iter().collect();
    emit_scoped_declaration(
        builder,
        ScopedEmission {
            symbol: SymbolEmission {
                node,
                name,
                body: node.child_by_field_name("body"),
                signature: None,
                shape: DeclarationShape::plain(SymbolKind::Class, true),
            },
            children: &body,
            depth,
        },
    )?;
    Ok(true)
}

/// The identifier a call invokes when it has no receiver.
fn bare_callee(call: Node<'_>) -> Option<Node<'_>> {
    let function = call.child_by_field_name("function")?;
    let mut parts = named_children(function);
    let identifier = parts.next().filter(|part| part.kind() == "identifier")?;
    parts.next().is_none().then_some(identifier)
}

/// Source text of a callee, borrowed from the snapshot.
fn callee_text<'source>(
    builder: &ExtractionBuilder<'source, '_>,
    callee: Node<'_>,
) -> &'source str {
    source_text(builder.context.snapshot.source(), callee).trim()
}

/// Scope key recording that a contract declares a callable named `name`.
fn member_key(contract: &str, name: &str) -> String {
    format!("{CONTRACT_MEMBER_SCOPE_PREFIX}{contract}::{name}")
}

/// Record every function and modifier a contract declares before its body is
/// visited, so calls that precede a declaration still see it.
fn register_contract_callables(
    builder: &mut ExtractionBuilder<'_, '_>,
    contract: Node<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let (Some(name), Some(body)) = (
        contract
            .child_by_field_name("name")
            .and_then(|name| bounded_name(source_text(source, name))),
        contract.child_by_field_name("body"),
    ) else {
        return Ok(());
    };
    let qualified = builder.qualified_name(name)?;
    for member in named_children(body) {
        builder.context.ensure_active()?;
        if !matches!(member.kind(), "function_definition" | "modifier_definition") {
            continue;
        }
        if let Some(member) = member
            .child_by_field_name("name")
            .and_then(|member| bounded_name(source_text(source, member)))
        {
            register_scope_key(builder, &member_key(&qualified, member), None)?;
        }
    }
    Ok(())
}

/// One emitted callable whose parameters and locals shadow contract members.
#[derive(Clone, Copy)]
struct LocalScope<'tree, 'id> {
    callable: Node<'tree>,
    id: &'id SymbolId,
}

/// Scope key recording that one callable occurrence binds a parameter or
/// local `name`. Keying by symbol identity keeps overloads apart.
fn local_key(callable: &SymbolId, name: &str) -> String {
    format!("{LOCAL_SCOPE_PREFIX}{}::{name}", callable.as_str())
}

/// Record the parameter, named return, and local variable names a callable binds, which
/// shadow same-named contract members inside its body.
fn register_local_names(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: LocalScope<'_, '_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    // Named return parameters bind names in the body exactly like parameters.
    let returns = input
        .callable
        .child_by_field_name("return_type")
        .into_iter()
        .flat_map(named_children);
    let bindings = named_children(input.callable)
        .chain(returns)
        .filter(|child| child.kind() == "parameter")
        .chain(
            input
                .callable
                .child_by_field_name("body")
                .into_iter()
                .flat_map(descendants)
                .filter(|node| node.kind() == "variable_declaration"),
        );
    for binding in bindings {
        builder.context.ensure_active()?;
        if let Some(name) = binding
            .child_by_field_name("name")
            .and_then(|name| bounded_name(source_text(source, name)))
        {
            register_scope_key(builder, &local_key(input.id, name), None)?;
        }
    }
    Ok(())
}

/// Whether the innermost enclosing callable binds `name` itself.
fn shadowed_by_local(builder: &ExtractionBuilder<'_, '_>, name: &str) -> bool {
    builder
        .native_owner_kinds
        .iter()
        .rposition(|kind| matches!(kind, SymbolKind::Method | SymbolKind::Function))
        .and_then(|depth| builder.owners.get(depth))
        .is_some_and(|callable| {
            builder
                .native_scope_symbols
                .contains_key(&local_key(callable, name))
        })
}

/// `Contract::name` when the innermost enclosing contract declares `name` and
/// no parameter or local variable of the calling function shadows it.
fn enclosing_contract_member(
    builder: &ExtractionBuilder<'_, '_>,
    callee: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(contract_depth) = builder
        .native_owner_kinds
        .iter()
        .rposition(|kind| METHOD_OWNER_KINDS.contains(kind))
    else {
        return Ok(None);
    };
    let Some(contract) = builder.qualifiers.get(..=contract_depth) else {
        return Ok(None);
    };
    let contract = contract.join("::");
    let name = callee_text(builder, callee);
    if bounded_name(name).is_none()
        || shadowed_by_local(builder, name)
        || !builder
            .native_scope_symbols
            .contains_key(&member_key(&contract, name))
    {
        return Ok(None);
    }
    let qualified = format!("{contract}::{name}");
    builder.context.copy_text(&qualified).map(Some)
}

/// `constructor`, `fallback`, or `receive`, taken from the keyword token.
fn special_callable_name(node: Node<'_>) -> Option<&'static str> {
    children(node).find_map(|child| match child.kind() {
        "constructor" => Some("constructor"),
        "fallback" => Some("fallback"),
        "receive" => Some("receive"),
        _ => None,
    })
}

/// Declared visibility; `external` functions are part of the public interface.
fn declared_visibility(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<Visibility> {
    let visibility = named_children(node).find(|child| child.kind() == "visibility")?;
    match builder.context.text(visibility).trim() {
        "public" | "external" => Some(Visibility::Public),
        "private" => Some(Visibility::Private),
        "internal" => Some(Visibility::Internal),
        _ => None,
    }
}

/// Kind, visibility, and export of a contract member.
fn member_shape(kind: SymbolKind, visibility: Option<Visibility>) -> DeclarationShape {
    DeclarationShape {
        kind,
        exported: !matches!(visibility, Some(Visibility::Private | Visibility::Internal)),
        visibility,
        async_symbol: false,
        declaration_only: false,
    }
}

/// One function-like definition and its resolved name.
#[derive(Clone, Copy)]
struct CallableVisit<'tree, 'name> {
    node: Node<'tree>,
    name: Option<&'name str>,
    depth: usize,
}

/// A function, modifier, constructor, fallback, or receive definition.
fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: CallableVisit<'_, '_>,
) -> Result<bool, ExtractError> {
    let Some(name) = input.name else {
        return Ok(false);
    };
    let name = builder.context.copy_text(name)?;
    let kind = if builder
        .native_owner_kinds
        .last()
        .is_some_and(|owner| METHOD_OWNER_KINDS.contains(owner))
    {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let signature = callable_signature(builder, input.node)?;
    let body = input.node.child_by_field_name("body");
    let shape = DeclarationShape {
        declaration_only: body.is_none(),
        ..member_shape(kind, declared_visibility(builder, input.node))
    };
    let qualifier = builder.context.copy_text(&name)?;
    let id = emit_declaration(
        builder,
        SymbolEmission {
            node: input.node,
            name,
            body,
            signature,
            shape,
        },
    )?;
    register_local_names(
        builder,
        LocalScope {
            callable: input.node,
            id: &id,
        },
    )?;
    // Header modifier invocations and the body run inside the callable; the
    // parameters and return types only name types.
    let children: Vec<Node<'_>> = named_children(input.node)
        .filter(|child| child.kind() == "modifier_invocation" || Some(*child) == body)
        .collect();
    visit_in_scope(
        builder,
        ScopeVisit {
            owner: &id,
            kind,
            name: &qualifier,
            children: &children,
            depth: input.depth,
        },
    )?;
    Ok(true)
}

/// Separator between rendered parameters.
const PARAMETER_SEPARATOR: &str = ", ";
/// Separator between the parameter list and its `returns (...)` clause.
const RETURNS_SEPARATOR: &str = " ";
/// Bytes of the parentheses around a rendered parameter list.
const PARAMETER_PARENTHESES_BYTES: usize = "()".len();

/// `(<parameters>) returns (<results>)`, when bounded and literal-free.
fn callable_signature(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let parameter_nodes = || named_children(node).filter(|child| child.kind() == "parameter");
    let returns = node
        .child_by_field_name("return_type")
        .map(|returns| builder.context.text(returns).trim());
    let length = parameter_nodes()
        .map(|parameter| {
            parameter
                .byte_range()
                .len()
                .saturating_add(PARAMETER_SEPARATOR.len())
        })
        .fold(
            returns.map_or(PARAMETER_PARENTHESES_BYTES, |returns| {
                returns
                    .len()
                    .saturating_add(PARAMETER_PARENTHESES_BYTES)
                    .saturating_add(RETURNS_SEPARATOR.len())
            }),
            usize::saturating_add,
        );
    if length > MAX_RETAINED_TEXT_BYTES {
        return Ok(None);
    }
    let parameters: Vec<&str> = parameter_nodes()
        .map(|parameter| builder.context.text(parameter).trim())
        .collect();
    let mut signature = format!("({})", parameters.join(PARAMETER_SEPARATOR));
    if let Some(returns) = returns {
        signature.push_str(RETURNS_SEPARATOR);
        signature.push_str(returns);
    }
    literal_free_signature(builder, &signature)
}

/// One enum value.
fn visit_enum_value(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(name) = bounded_name(builder.context.text(node)) else {
        return Ok(false);
    };
    let name = builder.context.copy_text(name)?;
    emit_declaration(
        builder,
        SymbolEmission {
            node,
            name,
            body: None,
            signature: None,
            shape: DeclarationShape::plain(SymbolKind::EnumMember, false),
        },
    )?;
    Ok(true)
}

/// A struct member or contract state variable; an initializer is visited
/// inside the field's scope so its calls keep their owner.
fn visit_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(name) = node
        .child_by_field_name("name")
        .and_then(|name| bounded_name(builder.context.text(name)))
    else {
        return Ok(false);
    };
    let signature = match node.child_by_field_name("type") {
        Some(field_type) => {
            let field_type = builder.context.text(field_type).trim();
            if field_type.len().saturating_add(name.len()) < MAX_RETAINED_TEXT_BYTES {
                literal_free_signature(builder, &format!("{field_type} {name}"))?
            } else {
                None
            }
        }
        None => None,
    };
    let name = builder.context.copy_text(name)?;
    let shape = member_shape(SymbolKind::Field, declared_visibility(builder, node));
    let shape = DeclarationShape {
        exported: shape.visibility == Some(Visibility::Public),
        ..shape
    };
    let value: Vec<Node<'_>> = node.child_by_field_name("value").into_iter().collect();
    emit_scoped_declaration(
        builder,
        ScopedEmission {
            symbol: SymbolEmission {
                node,
                name,
                body: None,
                signature,
                shape,
            },
            children: &value,
            depth,
        },
    )?;
    Ok(true)
}

/// A `pragma` directive is recorded as an import declaration named by its
/// text. It is not a module dependency, so no `Imports` reference is emitted.
fn visit_pragma(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let text = builder
        .context
        .text(node)
        .trim()
        .trim_end_matches(';')
        .trim_end();
    let valid = !text.is_empty()
        && text.len() <= MAX_RETAINED_TEXT_BYTES
        && !text
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | '\'' | '`'));
    if valid {
        let name = builder.context.copy_text(text)?;
        emit_import_symbol(builder, node, name)?;
    }
    Ok(true)
}
