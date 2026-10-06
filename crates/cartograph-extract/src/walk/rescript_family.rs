//! `ReScript` structural extraction.
//!
//! `ReScript` wraps declarations in binding nodes: `let_declaration > let_binding`,
//! `type_declaration > type_binding`, and `module_declaration > module_binding`.
//! A let binding whose value is a function literal is a function; variants and
//! records expand into enum members and fields; `open`/`include` import the full
//! module path; and a module alias references the module it renames.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::{Node, TreeCursor};

use crate::ExtractError;

use super::{
    ExtractionBuilder,
    family_support::{
        DeclarationShape, MAX_RETAINED_TEXT_BYTES, OwnedReference, ScopeVisit, SymbolEmission,
        bounded_name, emit_declaration, emit_import_reference, emit_owned_reference,
        emit_reference_owned_by, literal_free_signature, source_text, visit_in_scope, with_owner,
    },
    safe_assignment_signature,
    sql_family::OwnerScopeInput,
    syntax::{children, descendants_including_root, has_child_kind, named_children},
};

/// Node kinds naming a value or module path that a pipe targets.
const CALLABLE_PATH_KINDS: &[&str] = &["value_identifier", "value_identifier_path"];
/// Node kinds a call expression may invoke: a value, a module path, or a
/// record member such as `callbacks.onClick`.
const CALL_TARGET_KINDS: &[&str] = &[
    "value_identifier",
    "value_identifier_path",
    "member_expression",
];
/// Comments are named nodes, so child selection skips them explicitly.
const COMMENT_KIND: &str = "comment";
/// Deepest parenthesization unwrapped around an aliased module path or a callee.
const MAX_PARENTHESES: usize = 4;
/// Most proven record-member segments retained in a call target.
const MAX_CALL_PATH_DEPTH: usize = 16;
/// Node kinds naming the module an `open`, `include`, or alias refers to.
const MODULE_PATH_KINDS: &[&str] = &[
    "module_identifier",
    "module_identifier_path",
    "module_expression",
];

/// Dispatch one `ReScript` node; `false` lets the walker descend.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "open_statement" | "include_statement" => visit_open(builder, node),
        "type_declaration" | "let_declaration" => visit_bindings(builder, node, depth),
        "exception_declaration" => visit_named_child(
            builder,
            NamedChildDeclaration {
                node,
                name_kind: "variant_identifier",
                kind: SymbolKind::TypeAlias,
            },
        ),
        "external_declaration" => visit_named_child(
            builder,
            NamedChildDeclaration {
                node,
                name_kind: "value_identifier",
                kind: SymbolKind::Function,
            },
        ),
        "module_declaration" => visit_module_declaration(builder, node, depth),
        _ => Ok(false),
    }
}

/// Calls made by a call expression or a pipe into a bare function.
pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let target = match node.kind() {
        // `(helper)(x)` calls the parenthesized callee.
        "call_expression" => node
            .child_by_field_name("function")
            .map(|target| unparenthesized(target, "parenthesized_expression"))
            .filter(|target| CALL_TARGET_KINDS.contains(&target.kind())),
        // A call on the right of a pipe is captured as its own call expression.
        "pipe_expression" => named_children(node)
            .last()
            .filter(|target| CALLABLE_PATH_KINDS.contains(&target.kind())),
        _ => None,
    };
    let Some(target) = target else {
        return Ok(());
    };
    let Some(name) = call_target_name(builder, target, 0)? else {
        return Ok(());
    };
    emit_owned_reference(
        builder,
        OwnedReference {
            name: &name,
            kind: ReferenceKind::Calls,
            node: target,
        },
    )
}

/// Only identifier paths and members of proven paths can name a callee.
/// Calls and other computed record receivers are still visited for their uses.
fn call_target_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<Option<String>, ExtractError> {
    if depth > MAX_CALL_PATH_DEPTH {
        return Ok(None);
    }
    let node = unparenthesized(node, "parenthesized_expression");
    if CALLABLE_PATH_KINDS.contains(&node.kind()) {
        return bounded_name(builder.context.text(node))
            .filter(|name| name.split('.').all(identifier_text))
            .map(|name| builder.context.copy_text(name))
            .transpose();
    }
    if node.kind() != "member_expression" {
        return Ok(None);
    }
    member_call_target_name(builder, node, depth)
}

/// Inspect every child, including extras, before claiming the member path.
fn member_call_target_name(
    builder: &mut ExtractionBuilder<'_, '_>,
    member: Node<'_>,
    depth: usize,
) -> Result<Option<String>, ExtractError> {
    let mut cursor = member.walk();
    if !cursor.goto_first_child() {
        return Ok(None);
    }
    let mut name = None;
    let mut property = false;
    for _ in 0..MAX_CALL_PATH_DEPTH * 2 {
        builder.context.ensure_active()?;
        let child = cursor.node();
        if child.is_error() || child.is_missing() {
            return Ok(None);
        }
        match cursor.field_name() {
            Some("record") => {
                name = call_target_name(builder, child, depth.saturating_add(1))?;
            }
            Some("module" | "property") => {
                if !append_member_field(builder, &mut name, &cursor) {
                    return Ok(None);
                }
                property |= cursor.field_name() == Some("property");
            }
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            return Ok(name.filter(|_| property));
        }
    }
    Ok(None)
}

fn append_member_field(
    builder: &ExtractionBuilder<'_, '_>,
    name: &mut Option<String>,
    cursor: &TreeCursor<'_>,
) -> bool {
    let module = cursor.field_name() == Some("module");
    let child = cursor.node();
    if module && child.kind() == "." {
        return true;
    }
    let kind = if module {
        "module_identifier"
    } else {
        "property_identifier"
    };
    child.kind() == kind
        && name
            .as_mut()
            .is_some_and(|name| append_call_segment(builder, name, child))
}

fn append_call_segment(
    builder: &ExtractionBuilder<'_, '_>,
    name: &mut String,
    node: Node<'_>,
) -> bool {
    let Some(segment) =
        bounded_name(builder.context.text(node)).filter(|text| identifier_text(text))
    else {
        return false;
    };
    if name.len().saturating_add(segment.len()).saturating_add(1) > MAX_RETAINED_TEXT_BYTES {
        return false;
    }
    name.push('.');
    name.push_str(segment);
    true
}

fn identifier_text(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || unicode_ident::is_xid_start(first))
        && characters.all(unicode_ident::is_xid_continue)
}

/// Whether declarations at the current position are visible to other modules.
fn module_level(builder: &ExtractionBuilder<'_, '_>) -> bool {
    builder
        .native_owner_kinds
        .iter()
        .all(|kind| matches!(kind, SymbolKind::Namespace | SymbolKind::Interface))
}

/// Bounded text of the first direct named child of kind `kind`.
fn child_text(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    kind: &str,
) -> Result<Option<String>, ExtractError> {
    named_children(node)
        .find(|child| child.kind() == kind)
        .and_then(|child| bounded_name(builder.context.text(child)))
        .map(|text| builder.context.copy_text(text))
        .transpose()
}

/// An `open` or `include` imports its full module path.
fn visit_open(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(target) = named_children(node).find(|child| MODULE_PATH_KINDS.contains(&child.kind()))
    else {
        return Ok(false);
    };
    if let Some(module) = bounded_name(builder.context.text(target)) {
        let module = builder.context.copy_text(module)?;
        emit_import_reference(builder, target, module)?;
    }
    Ok(true)
}

/// One binding and its declaration span, determined once by its group.
#[derive(Clone, Copy)]
struct BindingNode<'tree> {
    node: Node<'tree>,
    span: Node<'tree>,
}

fn single_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    kind: &str,
) -> Result<bool, ExtractError> {
    let mut count = 0;
    for child in named_children(declaration) {
        builder.context.ensure_active()?;
        count += usize::from(child.kind() == kind);
        if count > 1 {
            return Ok(false);
        }
    }
    Ok(count == 1)
}

/// Determine multiplicity once, keeping documentation on a single binding.
fn visit_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let kind = if declaration.kind() == "type_declaration" {
        "type_binding"
    } else {
        "let_binding"
    };
    let single = single_binding(builder, declaration, kind)?;
    let child_depth = depth.saturating_add(1);
    for node in named_children(declaration) {
        builder.context.ensure_active()?;
        if child_depth > builder.maximum_ast_depth {
            return Err(ExtractError::NestingLimit);
        }
        let binding = BindingNode {
            node,
            span: if single { declaration } else { node },
        };
        let handled = match node.kind() {
            "type_binding" => visit_type_binding(builder, binding)?,
            "let_binding" => visit_let_binding(builder, binding, child_depth)?,
            _ => false,
        };
        if !handled {
            builder.visit(node, child_depth)?;
        }
    }
    Ok(true)
}

/// The shape of a type binding's definition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeShape {
    Variant,
    Record,
    Alias,
}

/// Classify a type binding as a variant, a record, or an alias.
fn type_shape(binding: Node<'_>) -> TypeShape {
    for child in named_children(binding) {
        match child.kind() {
            "variant_type" | "variant_declaration" => return TypeShape::Variant,
            "record_type" => return TypeShape::Record,
            _ => {}
        }
    }
    TypeShape::Alias
}

/// Emit a type binding with its variant constructors or record fields.
fn visit_type_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: BindingNode<'_>,
) -> Result<bool, ExtractError> {
    let binding = input.node;
    let Some(name_node) = binding.child_by_field_name("name") else {
        return Ok(false);
    };
    let Some(name) = bounded_name(builder.context.text(name_node)) else {
        return Ok(false);
    };
    let name = builder.context.copy_text(name)?;
    let shape = type_shape(binding);
    let kind = match shape {
        TypeShape::Variant => SymbolKind::Enum,
        TypeShape::Record => SymbolKind::Struct,
        TypeShape::Alias => SymbolKind::TypeAlias,
    };
    let qualifier = builder.context.copy_text(&name)?;
    let exported = module_level(builder);
    let owner = emit_declaration(
        builder,
        SymbolEmission {
            node: input.span,
            name,
            body: None,
            signature: None,
            shape: DeclarationShape::plain(kind, exported),
        },
    )?;
    let members = type_members(binding, shape);
    with_owner(
        builder,
        OwnerScopeInput {
            owner: &owner,
            kind,
            name: &qualifier,
        },
        |builder| {
            for (member, member_kind) in members {
                emit_type_member(builder, member, member_kind)?;
            }
            Ok(())
        },
    )?;
    Ok(true)
}

/// Variant constructors or record fields declared directly by a type binding.
fn type_members(binding: Node<'_>, shape: TypeShape) -> Vec<(Node<'_>, SymbolKind)> {
    let (container, member) = match shape {
        TypeShape::Variant => ("variant_type", "variant_declaration"),
        TypeShape::Record => ("record_type", "record_type_field"),
        TypeShape::Alias => return Vec::new(),
    };
    let kind = if shape == TypeShape::Variant {
        SymbolKind::EnumMember
    } else {
        SymbolKind::Field
    };
    named_children(binding)
        .flat_map(|child| {
            if child.kind() == container {
                named_children(child).collect()
            } else {
                vec![child]
            }
        })
        .filter(|child| child.kind() == member)
        .map(|child| (child, kind))
        .collect()
}

/// One variant constructor or record field.
fn emit_type_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    member: Node<'_>,
    kind: SymbolKind,
) -> Result<(), ExtractError> {
    let name_kind = if kind == SymbolKind::EnumMember {
        "variant_identifier"
    } else {
        "property_identifier"
    };
    let Some(name) = child_text(builder, member, name_kind)? else {
        return Ok(());
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
    Ok(())
}

/// A declaration whose name is a direct child of a given kind.
#[derive(Clone, Copy)]
struct NamedChildDeclaration<'tree> {
    node: Node<'tree>,
    name_kind: &'static str,
    kind: SymbolKind,
}

/// An exception (type alias) or external (function) named by a direct child.
fn visit_named_child(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamedChildDeclaration<'_>,
) -> Result<bool, ExtractError> {
    let Some(name) = child_text(builder, input.node, input.name_kind)? else {
        return Ok(false);
    };
    let signature = match named_children(input.node).find(|child| child.kind() == "type_annotation")
    {
        Some(annotation) if input.kind == SymbolKind::Function => {
            literal_free_signature(builder, builder.context.text(annotation))?
        }
        _ => None,
    };
    let exported = module_level(builder);
    emit_declaration(
        builder,
        SymbolEmission {
            node: input.node,
            name,
            body: None,
            signature,
            shape: DeclarationShape::plain(input.kind, exported),
        },
    )?;
    Ok(true)
}

/// A `let` binding names a function or a variable.
fn visit_let_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: BindingNode<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let binding = input.node;
    let Some(pattern) = binding
        .child_by_field_name("pattern")
        .filter(|pattern| pattern.kind() == "value_identifier")
    else {
        return Ok(false);
    };
    let Some(name) = bounded_name(builder.context.text(pattern)).filter(|name| *name != "_") else {
        return Ok(false);
    };
    let name = builder.context.copy_text(name)?;
    let input = LetBinding {
        binding,
        span: input.span,
        name: &name,
        value: binding.child_by_field_name("body"),
        depth,
    };
    match input.value.filter(|value| value.kind() == "function") {
        Some(function) => visit_let_function(builder, input, function)?,
        None => visit_let_value(builder, input)?,
    }
    Ok(true)
}

/// One identifier-named let binding and its optional value.
#[derive(Clone, Copy)]
struct LetBinding<'tree, 'name> {
    binding: Node<'tree>,
    span: Node<'tree>,
    name: &'name str,
    value: Option<Node<'tree>>,
    depth: usize,
}

/// A let binding whose value is a function literal.
fn visit_let_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: LetBinding<'_, '_>,
    function: Node<'_>,
) -> Result<(), ExtractError> {
    let signature = function_signature(builder, function)?;
    let exported = module_level(builder);
    let name = builder.context.copy_text(input.name)?;
    let owner = emit_declaration(
        builder,
        SymbolEmission {
            node: input.span,
            name,
            body: Some(function),
            signature,
            shape: DeclarationShape {
                kind: SymbolKind::Function,
                exported,
                visibility: None,
                async_symbol: has_child_kind(function, "async"),
                declaration_only: false,
            },
        },
    )?;
    let parameters = function
        .child_by_field_name("parameters")
        .or_else(|| function.child_by_field_name("parameter"));
    let result = function.child_by_field_name("return_type");
    for (root, kind) in [
        (parameters, ReferenceKind::TypeOf),
        (result, ReferenceKind::Returns),
        (result, ReferenceKind::TypeOf),
    ] {
        if let Some(root) = root {
            capture_types(
                builder,
                TypeCapture {
                    root,
                    owner: &owner,
                    kind,
                },
            )?;
        }
    }
    // Default values run when the function is called, so they are its calls.
    let mut body = parameters.map(default_values).unwrap_or_default();
    body.extend(function.child_by_field_name("body"));
    visit_in_scope(
        builder,
        ScopeVisit {
            owner: &owner,
            kind: SymbolKind::Function,
            name: input.name,
            children: &body,
            depth: input.depth,
        },
    )
}

/// The default-value expressions of `(~label=value)` parameters.
fn default_values(parameters: Node<'_>) -> Vec<Node<'_>> {
    named_children(parameters)
        .flat_map(named_children)
        .filter(|parameter| parameter.kind() == "labeled_parameter")
        .flat_map(|parameter| {
            let mut cursor = parameter.walk();
            parameter
                .children_by_field_name("default_value", &mut cursor)
                .filter(Node::is_named)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A type expression whose names are referenced by `owner`.
#[derive(Clone, Copy)]
struct TypeCapture<'tree, 'owner> {
    root: Node<'tree>,
    owner: &'owner SymbolId,
    kind: ReferenceKind,
}

/// Type names a type expression references. Type variables such as `'a` are
/// bound by the declaration itself and never name another declaration.
fn capture_types(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: TypeCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    for node in descendants_including_root(input.root) {
        builder.context.ensure_active()?;
        // A qualified `Module.t` names its whole path, never the bare `t`
        // that another module may also declare.
        let qualified_part = node
            .parent()
            .is_some_and(|parent| parent.kind() == "type_identifier_path");
        if !matches!(node.kind(), "type_identifier" | "type_identifier_path") || qualified_part {
            continue;
        }
        // `bounded_name` rejects quote characters, so type variables never pass.
        let Some(name) = bounded_name(source_text(source, node)) else {
            continue;
        };
        emit_reference_owned_by(
            builder,
            Some(input.owner.clone()),
            OwnedReference {
                name,
                kind: input.kind,
                node,
            },
        )?;
    }
    Ok(())
}

/// `(parameters): return` of a function literal, when literal-free.
///
/// `=>` is omitted because persisted callable signatures must not contain `=`.
fn function_signature(
    builder: &ExtractionBuilder<'_, '_>,
    function: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(parameters) = function
        .child_by_field_name("parameters")
        .or_else(|| function.child_by_field_name("parameter"))
    else {
        return Ok(None);
    };
    let parameters = builder.context.text(parameters).trim();
    let result = function
        .child_by_field_name("return_type")
        .map_or("", |result| builder.context.text(result).trim());
    if parameters.len().saturating_add(result.len()) > MAX_RETAINED_TEXT_BYTES {
        return Ok(None);
    }
    literal_free_signature(builder, &format!("{parameters}{result}"))
}

/// A let binding whose value is not a function literal.
fn visit_let_value(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: LetBinding<'_, '_>,
) -> Result<(), ExtractError> {
    let signature = match input.value {
        Some(value) => safe_assignment_signature(builder, value)?,
        None => None,
    };
    let exported = module_level(builder);
    let name = builder.context.copy_text(input.name)?;
    let owner = emit_declaration(
        builder,
        SymbolEmission {
            node: input.span,
            name,
            body: input.value,
            signature,
            // A binding without a value is a signature, as in a `.resi` file
            // or a module type; the implementation lives elsewhere.
            shape: DeclarationShape {
                declaration_only: input.value.is_none(),
                ..DeclarationShape::plain(SymbolKind::Variable, exported)
            },
        },
    )?;
    if let Some(annotation) =
        named_children(input.binding).find(|child| child.kind() == "type_annotation")
    {
        capture_types(
            builder,
            TypeCapture {
                root: annotation,
                owner: &owner,
                kind: ReferenceKind::TypeOf,
            },
        )?;
    }
    let value: Vec<Node<'_>> = input.value.into_iter().collect();
    visit_in_scope(
        builder,
        ScopeVisit {
            owner: &owner,
            kind: SymbolKind::Variable,
            name: input.name,
            children: &value,
            depth: input.depth,
        },
    )
}

/// A `module` or `module type` declaration with one or more bindings.
fn visit_module_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let kind = if children(declaration).any(|child| !child.is_named() && child.kind() == "type") {
        SymbolKind::Interface
    } else {
        SymbolKind::Namespace
    };
    let single = single_binding(builder, declaration, "module_binding")?;
    for node in named_children(declaration).filter(|child| child.kind() == "module_binding") {
        builder.context.ensure_active()?;
        let binding = BindingNode {
            node,
            span: if single { declaration } else { node },
        };
        visit_module_binding(builder, binding, ModuleKindDepth { kind, depth })?;
    }
    Ok(true)
}

/// Kind and traversal depth shared by every binding of one module declaration.
#[derive(Clone, Copy)]
struct ModuleKindDepth {
    kind: SymbolKind,
    depth: usize,
}

/// Emit one module binding and walk or reference its definition.
fn visit_module_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: BindingNode<'_>,
    module: ModuleKindDepth,
) -> Result<(), ExtractError> {
    let binding = input.node;
    let ModuleKindDepth { kind, depth } = module;
    let Some(name) = binding
        .child_by_field_name("name")
        .and_then(|name| bounded_name(builder.context.text(name)))
    else {
        return builder.visit_named_children(binding, depth);
    };
    let name = builder.context.copy_text(name)?;
    let qualifier = builder.context.copy_text(&name)?;
    let exported = module_level(builder);
    let owner = emit_declaration(
        builder,
        SymbolEmission {
            node: input.span,
            name,
            body: None,
            signature: None,
            shape: DeclarationShape::plain(kind, exported),
        },
    )?;
    let Some(definition) = binding
        .child_by_field_name("definition")
        .or_else(|| binding.child_by_field_name("signature"))
    else {
        return Ok(());
    };
    let scope = ModuleScope {
        owner: &owner,
        kind,
        name: &qualifier,
        depth,
    };
    visit_module_definition(builder, scope, definition)
}

/// An emitted module and the traversal context of its definition.
#[derive(Clone, Copy)]
struct ModuleScope<'scope> {
    owner: &'scope SymbolId,
    kind: SymbolKind,
    name: &'scope str,
    depth: usize,
}

/// The expression inside `(...)` parentheses of kind `wrapper`, such as a
/// `(Module.Path)` alias or a `(callee)`, unwrapped up to [`MAX_PARENTHESES`]
/// levels.
fn unparenthesized<'tree>(node: Node<'tree>, wrapper: &str) -> Node<'tree> {
    let mut current = node;
    for _ in 0..MAX_PARENTHESES {
        if current.kind() != wrapper {
            break;
        }
        match named_children(current).find(|inner| inner.kind() != COMMENT_KIND) {
            Some(inner) => current = inner,
            None => break,
        }
    }
    current
}

/// Walk a module body, a functor's body, or reference an aliased module.
fn visit_module_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: ModuleScope<'_>,
    definition: Node<'_>,
) -> Result<(), ExtractError> {
    let definition = unparenthesized(definition, "parenthesized_module_expression");
    if MODULE_PATH_KINDS.contains(&definition.kind()) {
        let Some(target) = bounded_name(builder.context.text(definition)) else {
            return Ok(());
        };
        let target = builder.context.copy_text(target)?;
        return with_owner(
            builder,
            OwnerScopeInput {
                owner: scope.owner,
                kind: scope.kind,
                name: scope.name,
            },
            |builder| {
                emit_owned_reference(
                    builder,
                    OwnedReference {
                        name: &target,
                        kind: ReferenceKind::References,
                        node: definition,
                    },
                )
            },
        );
    }
    let block = if definition.kind() == "functor" {
        definition.child_by_field_name("body")
    } else {
        Some(definition)
    };
    let children: Vec<Node<'_>> = block
        .filter(|block| block.kind() == "block")
        .map(|block| named_children(block).collect())
        .unwrap_or_default();
    visit_in_scope(
        builder,
        ScopeVisit {
            owner: scope.owner,
            kind: scope.kind,
            name: scope.name,
            children: &children,
            depth: scope.depth,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::super::syntax::DIRECT_CHILD_WORK;
    use crate::{NativeExtractor, SourceLimits, SourceSnapshot};
    use cartograph_domain::SymbolKind;

    fn group_work(width: usize, function: bool) -> usize {
        let (keyword, value) = if function {
            ("let rec", "() => ()")
        } else {
            ("type", "int")
        };
        let source = format!(
            "{keyword} {}\n",
            (0..width)
                .map(|index| format!("t{index} = {value}"))
                .collect::<Vec<_>>()
                .join(" and ")
        );
        let snapshot = SourceSnapshot::from_bytes(
            "Wide.res",
            source.as_bytes(),
            SourceLimits::new(1_048_576)
                .unwrap_or_else(|error| panic!("test setup failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("test setup failed: {error}"));
        DIRECT_CHILD_WORK.set(0);
        let extracted = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("test setup failed: {error}"))
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("test setup failed: {error}"));
        let expected = if function {
            SymbolKind::Function
        } else {
            SymbolKind::TypeAlias
        };
        assert_eq!(
            extracted
                .symbols
                .iter()
                .filter(|symbol| symbol.kind == expected)
                .count(),
            width
        );
        DIRECT_CHILD_WORK.get()
    }

    #[test]
    fn rescript_wide_binding_groups_measure_linear_ast_work() {
        for function in [false, true] {
            let work = group_work(96, function);
            let doubled = group_work(192, function);
            assert!(
                doubled < work * 3,
                "{function}: AST child visits grew from {work} to {doubled}"
            );
        }
    }
}
