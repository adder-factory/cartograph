//! Bounded grammar adapters for explicit nominal receiver declarations.

pub(super) mod bindings;
mod c_like;
pub(super) mod constructors;
mod cpp;
mod dart;
mod expressions;
mod fluent;
pub(super) mod returns;
mod scripts;
mod shadows;
mod type_bindings;

use super::{
    Bind, BindingType, ExtractError, ExtractionContext, MemberParts, MemberSite, NamedBind, Node,
    ReceiverTypes, ScopeKind, SourceLanguage, SymbolId, SyntaxIndex, TypeQuery, Visit, explicit,
    identifier, initializer, named_children, node_text, prefixed_type,
};

const MAX_TYPE_WRAPPERS: usize = 8;
const MAX_CHAIN_CALLS: usize = 3;

pub(super) fn reserve_slot<T>(
    context: &mut ExtractionContext<'_, '_>,
    values: &mut Vec<T>,
) -> Result<(), ExtractError> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let additional = values.capacity().max(1);
    context.budget.reserve_working_bytes(
        u64::try_from(additional)
            .map_err(|_| ExtractError::OutputLimit)?
            .saturating_mul(
                u64::try_from(std::mem::size_of::<T>()).map_err(|_| ExtractError::OutputLimit)?,
            ),
    )?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| ExtractError::OutputLimit)
}

pub(super) fn binding_key<'name>(
    context: &mut ExtractionContext<'_, '_>,
    name: &'name str,
) -> Result<std::borrow::Cow<'name, str>, ExtractError> {
    if !matches!(
        context.snapshot.language(),
        SourceLanguage::PowerShell | SourceLanguage::Pascal
    ) {
        return Ok(std::borrow::Cow::Borrowed(name));
    }
    super::reserve_text(context, name)?;
    let mut normalized = context.copy_text(name)?;
    normalized.make_ascii_lowercase();
    Ok(std::borrow::Cow::Owned(normalized))
}

pub(super) fn supported(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::Java
            | SourceLanguage::Kotlin
            | SourceLanguage::CSharp
            | SourceLanguage::Swift
            | SourceLanguage::Dart
            | SourceLanguage::Cpp
            | SourceLanguage::Scala
            | SourceLanguage::Ruby
            | SourceLanguage::Apex
            | SourceLanguage::Solidity
            | SourceLanguage::Ocaml
            | SourceLanguage::PowerShell
            | SourceLanguage::Pascal
            | SourceLanguage::ObjectiveC
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    )
}

pub(super) fn unsupported_site(
    context: &ExtractionContext<'_, '_>,
    reference: &super::ExtractedReference,
    site: MemberSite<'_>,
) -> bool {
    if !supported(context.snapshot.language()) {
        return false;
    }
    let self_receiver = matches!(node_text(context, site.receiver), "this" | "self" | "super");
    if reference.kind == super::ReferenceKind::Calls {
        return self_receiver;
    }
    !matches!(
        context.snapshot.language(),
        SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    ) || !self_receiver
}

pub(super) fn scope_kind(language: SourceLanguage, node: Node<'_>) -> Option<ScopeKind> {
    if matches!(
        language,
        SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    ) {
        return super::javascript::scope_kind(node)
            .map(|kind| {
                if kind == ScopeKind::Barrier {
                    ScopeKind::Callable
                } else {
                    kind
                }
            })
            .or_else(|| match node.kind() {
                "interface_declaration" | "enum_declaration" => Some(ScopeKind::Class),
                "statement_block" | "arrow_function" | "catch_clause" => Some(ScopeKind::Block),
                _ => None,
            });
    }
    match node.kind() {
        "class_definition" if language == SourceLanguage::Ocaml => None,
        "let_binding" if named_children(node).any(|node| node.kind() == "parameter") => {
            Some(ScopeKind::Callable)
        }
        "class_declaration"
        | "class_definition"
        | "class_specifier"
        | "struct_specifier"
        | "interface_declaration"
        | "record_declaration"
        | "annotation_type_declaration"
        | "protocol_declaration"
        | "trait_definition"
        | "trait_declaration"
        | "enum_declaration"
        | "enum_definition"
        | "enum_specifier"
        | "object_declaration"
        | "object_definition"
        | "delegate_declaration"
        | "associatedtype_declaration"
        | "struct_declaration"
        | "contract_declaration"
        | "class"
        | "class_binding"
        | "declType"
        | "class_statement"
        | "class_interface"
        | "class_implementation" => Some(ScopeKind::Class),
        "method_declaration"
        | "constructor_declaration"
        | "constructor_definition"
        | "method_definition"
        | "function_declaration"
        | "function_definition"
        | "method"
        | "singleton_method"
        | "function_statement"
        | "class_method_definition"
        | "defProc" => Some(ScopeKind::Callable),
        "function_body" if language == SourceLanguage::Dart => Some(ScopeKind::Callable),
        "block"
        | "compound_statement"
        | "for_statement"
        | "enhanced_for_statement"
        | "foreach_statement"
        | "for_range_loop"
        | "for_expression"
        | "catch_clause"
        | "catch_block"
        | "try_statement"
        | "lambda_expression"
        | "lambda_literal"
        | "template_declaration"
        | "let_expression"
        | "script_block"
        | "rescue"
        | "for"
        | "do_block"
        | "lambda" => block_scope(language, node),
        _ => None,
    }
}

fn block_scope(language: SourceLanguage, node: Node<'_>) -> Option<ScopeKind> {
    if language == SourceLanguage::PowerShell
        || language == SourceLanguage::Ruby && matches!(node.kind(), "for" | "rescue")
    {
        return None;
    }
    Some(ScopeKind::Block)
}

pub(super) fn unknown_this(language: SourceLanguage, node: Node<'_>) -> bool {
    matches!(
        language,
        SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    ) && super::javascript::scope_kind(node) == Some(ScopeKind::Barrier)
}

fn declaration_name(language: SourceLanguage, node: Node<'_>) -> Option<Node<'_>> {
    let kind = match language {
        SourceLanguage::Kotlin => "type_identifier",
        SourceLanguage::Ocaml => "class_name",
        SourceLanguage::PowerShell => "simple_name",
        SourceLanguage::ObjectiveC => "identifier",
        _ => return None,
    };
    named_children(node).find(|child| child.kind() == kind)
}

pub(super) fn bind_class(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, SymbolId),
) -> Result<(), ExtractError> {
    let (visit, nominal) = input;
    let Some(name) = visit
        .node
        .child_by_field_name("name")
        .or_else(|| declaration_name(context.snapshot.language(), visit.node))
    else {
        return Ok(());
    };
    let local = index
        .scopes
        .get(&visit.scope)
        .is_some_and(|scope| scope.kind != ScopeKind::Module);
    index.bind(
        context,
        Bind {
            scope: visit.scope,
            name,
            kind: BindingType::Nominal(nominal),
            start: if context.snapshot.language() == SourceLanguage::Python || local {
                visit.node.end_byte()
            } else {
                0
            },
        },
    )
}

pub(super) fn collect<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    type_bindings::collect(index, context, visit)?;
    shadows::collect(index, context, visit)?;
    constructors::collect(index, context, visit)?;
    returns::collect(index, context, visit)?;
    bindings::collect(index, context, visit)?;
    match context.snapshot.language() {
        SourceLanguage::Java
        | SourceLanguage::CSharp
        | SourceLanguage::Apex
        | SourceLanguage::Cpp
        | SourceLanguage::ObjectiveC
        | SourceLanguage::Solidity => c_like::collect(index, context, visit),
        SourceLanguage::Kotlin
        | SourceLanguage::Swift
        | SourceLanguage::Scala
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx
        | SourceLanguage::TypeScript
        | SourceLanguage::Tsx => fluent::collect(index, context, visit),
        SourceLanguage::Dart => dart::collect(index, context, visit),
        _ => scripts::collect(index, context, visit),
    }
}

/// Arrays, tuples, function types and generic containers are deliberately rejected.
fn type_node(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_TYPE_WRAPPERS {
        match node.kind() {
            "identifier"
            | "type_identifier"
            | "scoped_type_identifier"
            | "qualified_name"
            | "class_name"
            | "class_path"
            | "typeref"
            | "type_name" => return Some(node),
            "type_annotation" | "user_type" | "nullable_type" | "optional_type" | "type"
            | "user_defined_type" | "type_literal" | "type_spec" => {
                let mut children = named_children(node);
                node = children.next()?;
                if children.next().is_some() {
                    return None;
                }
            }
            // C++ template receivers retain the explicit outer class, never an argument.
            "template_type" => return node.child_by_field_name("name"),
            _ => return None,
        }
    }
    None
}

fn bind_typed(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, Option<Node<'_>>, Option<Node<'_>>),
) -> Result<(), ExtractError> {
    let (visit, name, annotation) = input;
    let Some(name) = name else {
        return Ok(());
    };
    let annotation = annotation.filter(|_| visit.node.child_by_field_name("dimensions").is_none());
    let kind = explicit(context, annotation.and_then(type_node), visit.scope)?;
    bind_kind(index, context, (visit, name, kind))
}

fn bind_kind(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, Node<'_>, BindingType),
) -> Result<(), ExtractError> {
    let (visit, name, kind) = input;
    if !identifier(node_text(context, name).trim()) {
        return index.fence(context, visit.scope);
    }
    let class = index
        .scopes
        .get(&visit.scope)
        .is_some_and(|scope| scope.kind == ScopeKind::Class);
    let start = if class || parameter(visit.node.kind()) {
        0
    } else {
        visit.node.end_byte()
    };
    index.bind(
        context,
        Bind {
            scope: visit.scope,
            name,
            kind,
            start,
        },
    )
}

fn parameter(kind: &str) -> bool {
    matches!(
        kind,
        "parameter"
            | "formal_parameter"
            | "parameter_declaration"
            | "class_parameter"
            | "required_parameter"
            | "optional_parameter"
            | "declArg"
            | "script_parameter"
    )
}

fn initialized(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, Option<Node<'_>>, Option<Node<'_>>),
) -> Result<(), ExtractError> {
    let (visit, name, value) = input;
    let Some(name) = name else {
        return Ok(());
    };
    let constructed =
        value.and_then(|value| expressions::constructor(context.snapshot.language(), value));
    let kind = initializer(context, constructed, visit.scope)?;
    bind_kind(index, context, (visit, name, kind))
}

pub(super) fn expression_type(
    types: &ReceiverTypes<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (MemberSite<'_>, usize),
) -> Result<Option<String>, ExtractError> {
    expressions::expression_type(types, context, input)
}

pub(super) fn managed_call_name(
    builder: &mut super::ExtractionBuilder<'_, '_>,
    target: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some((receiver, member)) = expressions::member_parts(target) else {
        return Ok(None);
    };
    if !matches!(
        receiver.kind(),
        "invocation_expression" | "object_creation_expression"
    ) {
        return Ok(None);
    }
    let name = node_text(&builder.context, member);
    if !identifier(name) {
        return Ok(None);
    }
    builder.context.copy_text(name).map(Some)
}

fn bind_unknown(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, Option<Node<'_>>),
) -> Result<(), ExtractError> {
    let (visit, name) = input;
    let Some(name) = name else {
        return Ok(());
    };
    if name.kind() == "unit" {
        return Ok(());
    }
    bind_kind(index, context, (visit, name, BindingType::Unknown))
}

fn member<'tree>(
    index: &mut SyntaxIndex<'tree>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'tree>, Option<Node<'tree>>, Option<Node<'tree>>),
) -> Result<(), ExtractError> {
    let (visit, receiver, member) = input;
    index.member_sites(
        context,
        MemberParts {
            visit,
            receiver,
            member,
        },
    )
}
