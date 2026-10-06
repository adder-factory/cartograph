use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind, Visibility};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, SingleChildUnwrap, references, rust_macro,
    syntax::{
        children, descendants_including_root, has_child_kind, is_call_or_construction_target,
        is_rust_turbofish_callee, named_children, span_for,
    },
};

mod go_members;
mod go_named_types;
mod go_reads;
mod import_index;
mod parameter_bindings;
mod python_import_scopes;
mod python_members;
mod qualified_path;
mod rust_attributes;
mod rust_members;
mod rust_reads;

pub(super) use python_import_scopes::fence_import_uses as fence_python_import_uses;
pub(super) use rust_reads::bound_by_enclosing_scope as rust_constant_bound_by_enclosing_scope;
mod type_targets;

/// Per-file lookup state of the polyglot walk, so that per-occurrence
/// questions (is this name a parameter, an import?) stay constant-time.
#[derive(Default)]
pub(super) struct PolyglotIndex {
    /// Names each enclosing scope's parameters bind, computed once per scope.
    parameter_bindings: parameter_bindings::ParameterBindings,
    /// How the file's imports bind each local name.
    imports: import_index::ImportIndex,
    /// The file's package-level Go map, slice, and array type names.
    go_container_types: go_named_types::GoContainerTypes,
}

const RUST_PARAMETER_UNWRAP: SingleChildUnwrap = SingleChildUnwrap::new(
    rust_parameter_identifier,
    &["captured_pattern", "mut_pattern", "reference_pattern"],
);

#[derive(Clone, Copy)]
struct ContainerDeclaration<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
    exported: bool,
    visibility: Option<Visibility>,
}

#[derive(Clone, Copy)]
struct CallableDeclaration<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
    exported: bool,
    async_symbol: bool,
    static_member: bool,
    visibility: Option<Visibility>,
}

#[derive(Clone, Copy)]
struct LeafDeclaration<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
    exported: bool,
    visibility: Option<Visibility>,
}

struct PolyglotNodeReference<'tree> {
    owner: Option<SymbolId>,
    node: Node<'tree>,
    kind: ReferenceKind,
}

struct OwnedBody<'tree> {
    owner: SymbolId,
    qualifier: String,
    body: Node<'tree>,
    depth: usize,
}

struct ContainerScope<'tree> {
    declaration: ContainerDeclaration<'tree>,
    owner: SymbolId,
    qualifier: String,
}

struct CallableSymbolInput<'tree> {
    declaration: CallableDeclaration<'tree>,
    name: String,
    body: Option<Node<'tree>>,
}

struct LeafSymbolInput<'tree> {
    declaration: LeafDeclaration<'tree>,
    name: String,
}

struct CallableScope<'tree> {
    declaration: CallableDeclaration<'tree>,
    owner: Option<SymbolId>,
    qualifier: Option<String>,
}

struct ReceiverOwner {
    symbol: Option<SymbolId>,
    name: Option<String>,
}

struct PolyglotImport<'tree> {
    node: Node<'tree>,
    module_specifier: String,
    kind: ImportBindingKind,
    imported_name: String,
    local_name: String,
    binding_node: Node<'tree>,
}

struct PythonImportName<'tree> {
    imported: Node<'tree>,
    name_node: Node<'tree>,
    name: String,
    alias: Option<String>,
}

struct PythonFromBinding<'tree, 'text> {
    binding: PythonImportName<'tree>,
    module_specifier: &'text str,
    package_prefix: Option<&'text str>,
}

#[derive(Clone, Copy)]
struct RustUseTraversal<'tree, 'text> {
    node: Node<'tree>,
    prefix: &'text str,
    depth: usize,
    re_export: bool,
}

struct RustNamespaceBinding<'tree> {
    binding_node: Node<'tree>,
    module_specifier: String,
    local_name: String,
    re_export: bool,
}

/// Bytes that can only enter a Go or Python dotted callee through a comment
/// (`/*..*/` or `//` in Go, `#` in Python), which its lookup name drops.
const LAYOUT_COMMENT_BYTES: [u8; 2] = *b"/#";

const MAX_RUST_USE_DEPTH: usize = 64;
const RUST_PATH_SEPARATOR: &str = "::";

#[derive(Clone, Copy)]
struct GoTypeDeclaration<'tree> {
    node: Node<'tree>,
    depth: usize,
    alias: bool,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Rust => visit_rust_declaration(builder, node, depth),
        SourceLanguage::Python => visit_python_declaration(builder, node, depth),
        SourceLanguage::Go => visit_go_declaration(builder, node, depth),
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => Ok(false),
        _ => Err(ExtractError::UnsupportedLanguage),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match builder.context.snapshot.language() {
        SourceLanguage::Rust => capture_rust_usage(builder, node),
        SourceLanguage::Go => capture_go_usage(builder, node),
        SourceLanguage::Python => capture_python_usage(builder, node),
        _ => Ok(()),
    }
}

fn capture_rust_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "call_expression" => {
            references::capture_invocation(builder, node, references::InvocationKind::Call)
        }
        "field_expression" => references::capture_member_field(builder, node, "field"),
        "scoped_identifier" => capture_rust_value_path(builder, node),
        "macro_invocation" => rust_macro::capture_invocation(builder, node),
        "macro_definition" => rust_macro::record_definition(builder, node),
        "struct_expression" => rust_members::capture_struct_expression(builder, node),
        "identifier" => rust_reads::capture_constant_read(builder, node),
        _ => Ok(()),
    }
}

fn capture_go_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "call_expression" => {
            if go_members::capture_cgo_call(builder, node)?
                || capture_layout_path_call(builder, node)?
            {
                return Ok(());
            }
            references::capture_invocation(builder, node, references::InvocationKind::Call)
        }
        "selector_expression" => references::capture_member_field(builder, node, "field"),
        "composite_literal" => go_members::capture_composite_literal(builder, node),
        "identifier" => go_reads::capture_constant_read(builder, node),
        _ => Ok(()),
    }
}

fn capture_python_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "call" => {
            if capture_layout_path_call(builder, node)? {
                return Ok(());
            }
            references::capture_invocation(builder, node, references::InvocationKind::Call)
        }
        "attribute" => references::capture_member_field(builder, node, "attribute"),
        _ => Ok(()),
    }
}

/// Record a call whose callee is a plain dotted path written with layout or a
/// comment inside it (`r. POST(..)`, `obj .method(..)`) under its lookup name
/// (`r.POST`), the name the same call written without layout carries, so a
/// member name never keeps the whitespace before it. Returns whether the call
/// was recorded; any other callee, including one too wide for a durable name,
/// is left to the general invocation capture.
fn capture_layout_path_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(callee) = call.child_by_field_name("function").filter(|callee| {
        matches!(callee.kind(), "selector_expression" | "attribute")
            && callee.end_byte().saturating_sub(callee.start_byte())
                <= references::MAX_DURABLE_REFERENCE_NAME_BYTES
    }) else {
        return Ok(false);
    };
    let has_layout = builder
        .context
        .text(callee)
        .trim()
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || LAYOUT_COMMENT_BYTES.contains(&byte));
    if !has_layout {
        return Ok(false);
    }
    let Some(name) = qualified_path::lookup_name(builder, callee)? else {
        return Ok(false);
    };
    references::push_reference(
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

fn visit_rust_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let emitted_before = builder.facts.symbols.len();
    let visited = if visit_rust_standard_declaration(builder, node, depth)? {
        true
    } else {
        rust_members::capture_field_types(builder, node)?;
        visit_rust_special_declaration(builder, node, depth)?
    };
    if visited {
        capture_rust_item_attributes(builder, node, emitted_before)?;
    }
    Ok(visited)
}

/// Record the outer attributes of an item as decorating the symbol its visit
/// emitted first; an item that declared no symbol, or only an import (an
/// out-of-line `mod name;`), has nothing for them to decorate.
fn capture_rust_item_attributes(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    emitted_before: usize,
) -> Result<(), ExtractError> {
    if !rust_attributes::decorates_item(node.kind()) {
        return Ok(());
    }
    let Some(owner) = symbol_emitted_since(builder, emitted_before, node) else {
        return Ok(());
    };
    let declares_import = builder
        .facts
        .symbols
        .get(emitted_before)
        .is_some_and(|symbol| symbol.kind == SymbolKind::Import);
    if declares_import {
        return Ok(());
    }
    rust_attributes::capture_item_attributes(builder, node, &owner)
}

#[derive(Clone, Copy)]
enum RustDeclarationKind {
    Container(SymbolKind),
    Leaf(SymbolKind),
}

fn visit_rust_standard_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let kind = rust_container_kind(node.kind())
        .map(RustDeclarationKind::Container)
        .or_else(|| rust_leaf_kind(node.kind()).map(RustDeclarationKind::Leaf));
    let Some(kind) = kind else {
        return Ok(false);
    };
    let visibility = rust_visibility(builder, node);
    match kind {
        RustDeclarationKind::Container(kind) => {
            let owner = visit_named_container(
                builder,
                ContainerDeclaration {
                    node,
                    depth,
                    kind,
                    exported: visibility.is_some(),
                    visibility,
                },
            )?;
            if kind == SymbolKind::Trait {
                rust_members::capture_supertraits(builder, node, &owner)?;
            }
        }
        RustDeclarationKind::Leaf(kind) => visit_leaf_declaration(
            builder,
            LeafDeclaration {
                node,
                depth,
                kind,
                exported: visibility.is_some(),
                visibility,
            },
        )?,
    }
    Ok(true)
}

fn visit_rust_special_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "mod_item" => visit_rust_module(builder, node, depth)?,
        "impl_item" => visit_rust_impl(builder, node, depth)?,
        "function_item" | "function_signature_item" => visit_rust_callable(builder, node, depth)?,
        "enum_variant" => visit_leaf_declaration(
            builder,
            LeafDeclaration {
                node,
                depth,
                kind: SymbolKind::EnumMember,
                exported: false,
                visibility: None,
            },
        )?,
        "use_declaration" => visit_rust_use(builder, node)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn rust_container_kind(node_kind: &str) -> Option<SymbolKind> {
    [
        ("struct_item", SymbolKind::Struct),
        ("enum_item", SymbolKind::Enum),
        ("trait_item", SymbolKind::Trait),
    ]
    .into_iter()
    .find_map(|(candidate, kind)| (candidate == node_kind).then_some(kind))
}

fn rust_leaf_kind(node_kind: &str) -> Option<SymbolKind> {
    [
        ("type_item", SymbolKind::TypeAlias),
        ("associated_type", SymbolKind::TypeAlias),
        ("const_item", SymbolKind::Constant),
        ("static_item", SymbolKind::Variable),
    ]
    .into_iter()
    .find_map(|(candidate, kind)| (candidate == node_kind).then_some(kind))
}

fn visit_rust_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if node.child_by_field_name("body").is_none() {
        return visit_rust_external_module(builder, node);
    }
    let visibility = rust_visibility(builder, node);
    visit_named_container(
        builder,
        ContainerDeclaration {
            node,
            depth,
            kind: SymbolKind::Module,
            exported: visibility.is_some(),
            visibility,
        },
    )
    .map(|_| ())
}

fn visit_rust_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let kind = if is_rust_associated_callable(node) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let visibility = rust_visibility(builder, node);
    visit_callable(
        builder,
        CallableDeclaration {
            node,
            depth,
            kind,
            exported: visibility.is_some(),
            async_symbol: rust_async(node),
            static_member: false,
            visibility,
        },
    )
}

fn visit_python_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "class_definition" => visit_python_class(builder, node, depth)?,
        "function_definition" => visit_python_function(builder, node, depth)?,
        "assignment" => return python_members::visit_assignment(builder, node, depth),
        "import_statement" | "import_from_statement" => visit_python_import(builder, node)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_python_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let kind = if is_python_class_member(node) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let exported = node
        .child_by_field_name("name")
        .is_some_and(|name| python_exported(builder, name));
    let emitted_before = builder.facts.symbols.len();
    visit_callable(
        builder,
        CallableDeclaration {
            node,
            depth,
            kind,
            exported,
            async_symbol: has_child_kind(node, "async"),
            static_member: python_members::is_static_method(builder, node),
            visibility: None,
        },
    )?;
    let Some(owner) = symbol_emitted_since(builder, emitted_before, node) else {
        return Ok(());
    };
    python_members::capture_decorators(builder, node, &owner)
}

/// The symbol a declaration visit emitted for `node`: the first symbol pushed
/// since `emitted_before` whose span starts at the declaration.
fn symbol_emitted_since(
    builder: &ExtractionBuilder<'_, '_>,
    emitted_before: usize,
    node: Node<'_>,
) -> Option<SymbolId> {
    let start = u64::try_from(node.start_byte()).ok()?;
    builder
        .facts
        .symbols
        .get(emitted_before)
        .filter(|symbol| symbol.span.start_byte() == start)
        .map(|symbol| symbol.id.clone())
}

fn visit_go_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if go_members::visit_declaration(builder, node, depth)? {
        return Ok(true);
    }
    match node.kind() {
        "package_clause" => {
            visit_go_package(builder, node)?;
        }
        "function_declaration" => {
            let exported = node
                .child_by_field_name("name")
                .is_some_and(|name| go_exported(builder, name));
            visit_callable(
                builder,
                CallableDeclaration {
                    node,
                    depth,
                    kind: SymbolKind::Function,
                    exported,
                    async_symbol: false,
                    static_member: false,
                    visibility: None,
                },
            )?;
        }
        "method_declaration" => visit_go_method(builder, node, depth)?,
        "method_elem" => {
            let exported = node
                .child_by_field_name("name")
                .is_some_and(|name| go_exported(builder, name));
            visit_callable(
                builder,
                CallableDeclaration {
                    node,
                    depth,
                    kind: SymbolKind::Method,
                    exported,
                    async_symbol: false,
                    static_member: false,
                    visibility: None,
                },
            )?;
        }
        "type_declaration" => builder.visit_named_children(node, depth)?,
        "type_spec" => visit_go_type(
            builder,
            GoTypeDeclaration {
                node,
                depth,
                alias: false,
            },
        )?,
        "type_alias" => visit_go_type(
            builder,
            GoTypeDeclaration {
                node,
                depth,
                alias: true,
            },
        )?,
        "import_declaration" => visit_go_imports(builder, node)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn visit_named_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: ContainerDeclaration<'_>,
) -> Result<SymbolId, ExtractError> {
    let Some(name_node) = declaration.node.child_by_field_name("name") else {
        builder.visit_named_children(declaration.node, declaration.depth)?;
        return Err(ExtractError::InvalidSpan);
    };
    let name = builder.context.owned_text(name_node)?;
    let id = emit_container_symbol(builder, declaration, name.clone())?;
    visit_container_body(
        builder,
        ContainerScope {
            declaration,
            owner: id.clone(),
            qualifier: name,
        },
    )?;
    Ok(id)
}

fn visit_container_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: ContainerScope<'_>,
) -> Result<(), ExtractError> {
    let Some(body) = input.declaration.node.child_by_field_name("body") else {
        return Ok(());
    };
    visit_owned_body(
        builder,
        OwnedBody {
            owner: input.owner,
            qualifier: input.qualifier,
            body,
            depth: input.declaration.depth,
        },
    )
}

fn emit_container_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: ContainerDeclaration<'_>,
    name: String,
) -> Result<SymbolId, ExtractError> {
    let pending = PendingSymbol {
        kind: declaration.kind,
        name,
        span_node: declaration.node,
        structural_node: declaration.node,
        doc_anchor: declaration.node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(declaration.exported, false),
        async_symbol: false,
        static_member: false,
        visibility: declaration.visibility,
    };
    builder.emit_symbol(pending)
}

fn visit_python_class(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let exported = node
        .child_by_field_name("name")
        .is_some_and(|name| python_exported(builder, name));
    let id = visit_named_container(
        builder,
        ContainerDeclaration {
            node,
            depth,
            kind: SymbolKind::Class,
            exported,
            visibility: None,
        },
    )?;
    capture_python_heritage(builder, node, &id)?;
    python_members::capture_decorators(builder, node, &id)
}

fn capture_python_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(superclasses) = node.child_by_field_name("superclasses") else {
        return Ok(());
    };
    for target in named_children(superclasses)
        .filter(|target| matches!(target.kind(), "identifier" | "attribute"))
    {
        let kind = python_members::base_reference_kind(builder.context.text(target));
        emit_node_reference(
            builder,
            PolyglotNodeReference {
                owner: Some(owner.clone()),
                node: target,
                kind,
            },
        )?;
    }
    Ok(())
}

fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: CallableDeclaration<'_>,
) -> Result<(), ExtractError> {
    let Some(input) = callable_symbol_input(builder, declaration)? else {
        return builder.visit_named_children(declaration.node, declaration.depth);
    };
    let id = emit_callable_symbol(builder, &input)?;
    references::capture_callable_types(builder, declaration.node, &id)?;
    emit_rust_callable_parameters(
        builder,
        RustCallableParameters {
            callable: declaration.node,
            owner: &id,
            callable_name: &input.name,
        },
    )?;
    visit_callable_body(builder, input, id)
}

fn callable_symbol_input<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: CallableDeclaration<'tree>,
) -> Result<Option<CallableSymbolInput<'tree>>, ExtractError> {
    let Some(name_node) = declaration.node.child_by_field_name("name") else {
        return Ok(None);
    };
    Ok(Some(CallableSymbolInput {
        declaration,
        name: builder.context.owned_text(name_node)?,
        body: declaration.node.child_by_field_name("body"),
    }))
}

fn visit_callable_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: CallableSymbolInput<'_>,
    owner: SymbolId,
) -> Result<(), ExtractError> {
    let Some(body) = input.body else {
        return Ok(());
    };
    visit_owned_body(
        builder,
        OwnedBody {
            owner,
            qualifier: input.name,
            body,
            depth: input.declaration.depth,
        },
    )
}

fn emit_callable_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: &CallableSymbolInput<'_>,
) -> Result<SymbolId, ExtractError> {
    let pending = PendingSymbol {
        kind: input.declaration.kind,
        name: input.name.clone(),
        span_node: input.declaration.node,
        structural_node: input.declaration.node,
        doc_anchor: input.declaration.node,
        body_node: input.body,
        declaration_only: input.body.is_none(),
        signature: builder.context.callable_signature(input.declaration.node)?,
        export: crate::SymbolExportFlags::new(input.declaration.exported, false),
        async_symbol: input.declaration.async_symbol,
        static_member: input.declaration.static_member,
        visibility: input.declaration.visibility,
    };
    builder.emit_symbol(pending)
}

fn emit_rust_callable_parameters(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: RustCallableParameters<'_>,
) -> Result<(), ExtractError> {
    let RustCallableParameters {
        callable,
        owner,
        callable_name,
    } = input;
    if builder.context.snapshot.language() != SourceLanguage::Rust {
        return Ok(());
    }
    let Some(parameters) = callable.child_by_field_name("parameters") else {
        return Ok(());
    };
    let qualifier = builder.context.copy_text(callable_name)?;
    builder.owners.push(owner.clone());
    builder.qualifiers.push(qualifier);
    let result = emit_rust_parameters_in_scope(builder, parameters);
    builder.qualifiers.pop();
    builder.owners.pop();
    result
}

#[derive(Clone, Copy)]
struct RustCallableParameters<'a> {
    callable: Node<'a>,
    owner: &'a SymbolId,
    callable_name: &'a str,
}

fn emit_rust_parameters_in_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    parameters: Node<'_>,
) -> Result<(), ExtractError> {
    for parameter in named_children(parameters) {
        if parameter.kind() != "parameter" {
            continue;
        }
        let Some(pattern) = parameter
            .child_by_field_name("pattern")
            .or_else(|| named_children(parameter).next())
        else {
            continue;
        };
        let Some(identifier) = super::unwrap_single_child(pattern, 0, RUST_PARAMETER_UNWRAP) else {
            continue;
        };
        let name = builder.context.owned_text(identifier)?;
        builder.emit_symbol(PendingSymbol {
            kind: SymbolKind::Parameter,
            name,
            span_node: identifier,
            structural_node: parameter,
            doc_anchor: parameter,
            body_node: None,
            declaration_only: false,
            signature: None,
            export: crate::SymbolExportFlags::new(false, false),
            async_symbol: false,
            static_member: false,
            visibility: None,
        })?;
    }
    Ok(())
}

fn rust_parameter_identifier(node: Node<'_>) -> bool {
    node.kind() == "identifier"
}

fn visit_leaf_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: LeafDeclaration<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = declaration.node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let qualifier = builder.context.copy_text(&name)?;
    let value = declaration.node.child_by_field_name("value");
    let owner = emit_leaf_symbol(builder, LeafSymbolInput { declaration, name })?;
    let Some(body) = value else {
        return Ok(());
    };
    visit_owned_body(
        builder,
        OwnedBody {
            owner,
            qualifier,
            body,
            depth: declaration.depth,
        },
    )
}

fn emit_leaf_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: LeafSymbolInput<'_>,
) -> Result<SymbolId, ExtractError> {
    let pending = PendingSymbol {
        kind: input.declaration.kind,
        name: input.name,
        span_node: input.declaration.node,
        structural_node: input.declaration.node,
        doc_anchor: input.declaration.node,
        body_node: input.declaration.node.child_by_field_name("value"),
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(input.declaration.exported, false),
        async_symbol: false,
        static_member: false,
        visibility: input.declaration.visibility,
    };
    builder.emit_symbol(pending)
}

fn visit_owned_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: OwnedBody<'_>,
) -> Result<(), ExtractError> {
    builder.owners.push(input.owner);
    builder.qualifiers.push(input.qualifier);
    let result = builder.visit(input.body, input.depth.saturating_add(1));
    builder.qualifiers.pop();
    builder.owners.pop();
    result
}

fn visit_scoped_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: CallableScope<'_>,
) -> Result<(), ExtractError> {
    let CallableScope {
        declaration,
        owner,
        qualifier,
    } = input;
    let owner_depth = builder.owners.len();
    let qualifier_depth = builder.qualifiers.len();
    builder.owners.extend(owner);
    builder.qualifiers.extend(qualifier);
    let result = visit_callable(builder, declaration);
    builder.qualifiers.truncate(qualifier_depth);
    builder.owners.truncate(owner_depth);
    result
}

fn visit_rust_impl(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(type_node) = node.child_by_field_name("type") else {
        return builder.visit_named_children(node, depth);
    };
    let Some(type_name_node) = descendants_including_root(type_node)
        .find(|candidate| candidate.kind() == "type_identifier")
    else {
        return builder.visit_named_children(node, depth);
    };
    let type_name = builder.context.owned_text(type_name_node)?;
    let owner = top_level_symbol(builder, &type_name);
    if let Some(owner) = &owner {
        builder.owners.push(owner.clone());
    }
    builder.qualifiers.push(type_name);
    if let Some(trait_node) = node.child_by_field_name("trait") {
        emit_node_reference(
            builder,
            PolyglotNodeReference {
                owner: owner.clone(),
                node: trait_node,
                kind: ReferenceKind::Implements,
            },
        )?;
    }
    if let Some(body) = node.child_by_field_name("body") {
        builder.visit(body, depth.saturating_add(1))?;
    }
    builder.qualifiers.pop();
    if owner.is_some() {
        builder.owners.pop();
    }
    Ok(())
}

fn visit_rust_external_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let local_name = builder.context.owned_text(name_node)?;
    let module_specifier = rust_external_module_specifier(builder, &local_name)?;
    if !builder.owners.is_empty() {
        return emit_import_symbol_and_reference(builder, node, module_specifier);
    }
    emit_import(
        builder,
        PolyglotImport {
            node,
            module_specifier,
            kind: ImportBindingKind::Namespace,
            imported_name: "*".to_owned(),
            local_name,
            binding_node: name_node,
        },
    )
}

fn rust_external_module_specifier(
    builder: &ExtractionBuilder<'_, '_>,
    local_name: &str,
) -> Result<String, ExtractError> {
    let file_name = builder
        .context
        .snapshot
        .path()
        .as_str()
        .rsplit_once('/')
        .map_or(builder.context.snapshot.path().as_str(), |(_, name)| name);
    let parent_module = match file_name {
        "lib.rs" | "main.rs" | "mod.rs" => None,
        name => name.strip_suffix(".rs").filter(|name| !name.is_empty()),
    };
    let length = "./"
        .len()
        .checked_add(local_name.len())
        .and_then(|length| {
            parent_module.map_or(Some(length), |parent| {
                length
                    .checked_add(parent.len())
                    .and_then(|length| length.checked_add(1))
            })
        })
        .ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(length)?;
    let mut specifier = String::new();
    specifier
        .try_reserve_exact(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    specifier.push_str("./");
    if let Some(parent) = parent_module {
        specifier.push_str(parent);
        specifier.push('/');
    }
    specifier.push_str(local_name);
    Ok(specifier)
}

fn visit_rust_use(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(argument) = node.child_by_field_name("argument") else {
        return Ok(());
    };
    let raw = builder.context.owned_text(argument)?;
    let re_export = rust_visibility(builder, node) == Some(Visibility::Public);
    let wildcard_specifier = (raw.ends_with("::*") && builder.owners.is_empty())
        .then(|| builder.context.copy_text(&raw))
        .transpose()?;
    emit_import_symbol_and_reference(builder, node, raw)?;
    if let Some(module_specifier) = wildcard_specifier {
        return emit_rust_namespace_binding(
            builder,
            RustNamespaceBinding {
                binding_node: argument,
                module_specifier,
                local_name: "*".to_owned(),
                re_export,
            },
        );
    }
    emit_rust_use_bindings(
        builder,
        RustUseTraversal {
            node: argument,
            prefix: "",
            depth: 0,
            re_export,
        },
    )
}

fn emit_rust_use_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    traversal: RustUseTraversal<'_, '_>,
) -> Result<(), ExtractError> {
    walk_rust_use_bindings(builder, traversal, &mut emit_rust_namespace_binding)
}

pub(super) fn rust_nominal_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    scope: Node<'_>,
    local_name: &str,
) -> Result<Option<String>, ExtractError> {
    let mut target = None;
    let mut ambiguous = false;
    for declaration in named_children(scope) {
        builder.context.ensure_active()?;
        if declaration.kind() != "use_declaration" {
            continue;
        }
        let Some(argument) = declaration.child_by_field_name("argument") else {
            continue;
        };
        walk_rust_use_bindings(
            builder,
            RustUseTraversal {
                node: argument,
                prefix: "",
                depth: 0,
                re_export: false,
            },
            &mut |_, binding| {
                if binding.local_name == local_name {
                    ambiguous |= target.is_some();
                    target = Some(binding.module_specifier);
                }
                Ok(())
            },
        )?;
    }
    Ok(if ambiguous { None } else { target })
}

fn walk_rust_use_bindings<Emit>(
    builder: &mut ExtractionBuilder<'_, '_>,
    traversal: RustUseTraversal<'_, '_>,
    emit: &mut Emit,
) -> Result<(), ExtractError>
where
    Emit:
        FnMut(&mut ExtractionBuilder<'_, '_>, RustNamespaceBinding<'_>) -> Result<(), ExtractError>,
{
    builder.context.ensure_active()?;
    if traversal.depth > MAX_RUST_USE_DEPTH {
        return Err(ExtractError::NestingLimit);
    }
    match traversal.node.kind() {
        "scoped_use_list" => emit_scoped_rust_use_bindings(builder, traversal, emit),
        "use_list" => emit_rust_use_list_bindings(builder, traversal, emit),
        "use_as_clause" => emit_aliased_rust_use_binding(builder, traversal, emit),
        "identifier" | "scoped_identifier" => emit_rust_path_binding(builder, traversal, emit),
        "self" if !traversal.prefix.is_empty() => emit_rust_self_binding(builder, traversal, emit),
        "use_wildcard" if !traversal.prefix.is_empty() && builder.owners.is_empty() => {
            let module_specifier = join_rust_use_path(builder, traversal.prefix, "*")?;
            emit(
                builder,
                RustNamespaceBinding {
                    binding_node: traversal.node,
                    module_specifier,
                    local_name: "*".to_owned(),
                    re_export: traversal.re_export,
                },
            )
        }
        _ => Ok(()),
    }
}

fn emit_rust_use_list_bindings<Emit>(
    builder: &mut ExtractionBuilder<'_, '_>,
    traversal: RustUseTraversal<'_, '_>,
    emit: &mut Emit,
) -> Result<(), ExtractError>
where
    Emit:
        FnMut(&mut ExtractionBuilder<'_, '_>, RustNamespaceBinding<'_>) -> Result<(), ExtractError>,
{
    for child in named_children(traversal.node) {
        walk_rust_use_bindings(
            builder,
            RustUseTraversal {
                node: child,
                prefix: traversal.prefix,
                depth: traversal.depth.saturating_add(1),
                re_export: traversal.re_export,
            },
            emit,
        )?;
    }
    Ok(())
}

fn emit_scoped_rust_use_bindings<Emit>(
    builder: &mut ExtractionBuilder<'_, '_>,
    traversal: RustUseTraversal<'_, '_>,
    emit: &mut Emit,
) -> Result<(), ExtractError>
where
    Emit:
        FnMut(&mut ExtractionBuilder<'_, '_>, RustNamespaceBinding<'_>) -> Result<(), ExtractError>,
{
    let RustUseTraversal {
        node,
        prefix,
        depth,
        re_export,
    } = traversal;
    let Some(path_node) = node.child_by_field_name("path") else {
        return Ok(());
    };
    let Some(list) = node.child_by_field_name("list") else {
        return Ok(());
    };
    let path = builder.context.owned_text(path_node)?;
    let scoped_prefix = join_rust_use_path(builder, prefix, &path)?;
    walk_rust_use_bindings(
        builder,
        RustUseTraversal {
            node: list,
            prefix: &scoped_prefix,
            depth: depth.saturating_add(1),
            re_export,
        },
        emit,
    )
}

fn emit_aliased_rust_use_binding<Emit>(
    builder: &mut ExtractionBuilder<'_, '_>,
    traversal: RustUseTraversal<'_, '_>,
    emit: &mut Emit,
) -> Result<(), ExtractError>
where
    Emit:
        FnMut(&mut ExtractionBuilder<'_, '_>, RustNamespaceBinding<'_>) -> Result<(), ExtractError>,
{
    let Some(path_node) = traversal.node.child_by_field_name("path") else {
        return Ok(());
    };
    let Some(alias_node) = traversal.node.child_by_field_name("alias") else {
        return Ok(());
    };
    let path = builder.context.owned_text(path_node)?;
    let module_specifier = join_rust_use_path(builder, traversal.prefix, &path)?;
    let local_name = builder.context.owned_text(alias_node)?;
    emit(
        builder,
        RustNamespaceBinding {
            binding_node: alias_node,
            module_specifier,
            local_name,
            re_export: traversal.re_export,
        },
    )
}

fn emit_rust_path_binding<Emit>(
    builder: &mut ExtractionBuilder<'_, '_>,
    traversal: RustUseTraversal<'_, '_>,
    emit: &mut Emit,
) -> Result<(), ExtractError>
where
    Emit:
        FnMut(&mut ExtractionBuilder<'_, '_>, RustNamespaceBinding<'_>) -> Result<(), ExtractError>,
{
    let path = builder.context.owned_text(traversal.node)?;
    let module_specifier = join_rust_use_path(builder, traversal.prefix, &path)?;
    let local_name = rust_use_local_name(&module_specifier).ok_or(ExtractError::OutputLimit)?;
    let local_name = builder.context.copy_text(local_name)?;
    emit(
        builder,
        RustNamespaceBinding {
            binding_node: traversal.node,
            module_specifier,
            local_name,
            re_export: traversal.re_export,
        },
    )
}

fn emit_rust_self_binding<Emit>(
    builder: &mut ExtractionBuilder<'_, '_>,
    traversal: RustUseTraversal<'_, '_>,
    emit: &mut Emit,
) -> Result<(), ExtractError>
where
    Emit:
        FnMut(&mut ExtractionBuilder<'_, '_>, RustNamespaceBinding<'_>) -> Result<(), ExtractError>,
{
    let local_name = rust_use_local_name(traversal.prefix).ok_or(ExtractError::OutputLimit)?;
    let local_name = builder.context.copy_text(local_name)?;
    let module_specifier = builder.context.copy_text(traversal.prefix)?;
    emit(
        builder,
        RustNamespaceBinding {
            binding_node: traversal.node,
            module_specifier,
            local_name,
            re_export: traversal.re_export,
        },
    )
}

fn emit_rust_namespace_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    binding: RustNamespaceBinding<'_>,
) -> Result<(), ExtractError> {
    let span = span_for(binding.binding_node)?;
    let reference_name = builder.context.copy_text(&binding.local_name)?;
    let imported_name = if binding.re_export {
        let imported_name =
            rust_use_local_name(&binding.module_specifier).ok_or(ExtractError::OutputLimit)?;
        builder.context.copy_text(imported_name)?
    } else {
        "*".to_owned()
    };
    let local_name = if binding.re_export && binding.local_name != "*" {
        builder.qualified_name(&binding.local_name)?
    } else {
        binding.local_name
    };
    builder.emit_import_binding(ExtractedImportBinding {
        kind: if binding.re_export {
            ImportBindingKind::ReExportNamed
        } else {
            ImportBindingKind::Namespace
        },
        module_specifier: binding.module_specifier,
        imported_name,
        local_name,
        span,
    })?;
    if reference_name == "*" {
        return Ok(());
    }
    builder.emit_reference(crate::ExtractedReference {
        owner: None,
        name: reference_name,
        resolution_name: None,
        kind: ReferenceKind::References,
        span,
    })
}

fn capture_rust_value_path(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if rust_value_path_named_elsewhere(node) || rust_members::names_instantiated_type(builder, node)
    {
        return Ok(());
    }
    emit_node_reference(
        builder,
        PolyglotNodeReference {
            owner: builder.owners.last().cloned(),
            node,
            kind: ReferenceKind::References,
        },
    )
}

/// Whether another reference already names this path: a call (turbofish
/// included) names its callee, a longer path names its segments, and a macro
/// invocation names its macro.
fn rust_value_path_named_elsewhere(node: Node<'_>) -> bool {
    is_call_or_construction_target(node)
        || is_rust_turbofish_callee(node)
        || node
            .parent()
            .is_some_and(|parent| rust_parent_names_path(parent, node))
}

/// Whether `parent` itself names `node`: a longer path containing it, or the
/// macro invocation whose macro it is.
fn rust_parent_names_path(parent: Node<'_>, node: Node<'_>) -> bool {
    match parent.kind() {
        "scoped_identifier" | "scoped_type_identifier" => true,
        "macro_invocation" => parent.child_by_field_name("macro").is_some_and(|target| {
            target.start_byte() == node.start_byte() && target.end_byte() == node.end_byte()
        }),
        _ => false,
    }
}

fn join_rust_use_path(
    builder: &ExtractionBuilder<'_, '_>,
    prefix: &str,
    suffix: &str,
) -> Result<String, ExtractError> {
    let context = &builder.context;
    if prefix.is_empty() {
        return context.copy_text(suffix);
    }
    let length = rust_use_path_length(prefix, suffix)?;
    context.budget.ensure_string_length(length)?;
    let mut path = context.copy_text(prefix)?;
    append_rust_use_component(&mut path, suffix)?;
    Ok(path)
}

fn rust_use_path_length(prefix: &str, suffix: &str) -> Result<usize, ExtractError> {
    prefix
        .len()
        .checked_add(RUST_PATH_SEPARATOR.len())
        .and_then(|length| length.checked_add(suffix.len()))
        .ok_or(ExtractError::OutputLimit)
}

fn append_rust_use_component(path: &mut String, component: &str) -> Result<(), ExtractError> {
    path.try_reserve_exact(component.len().saturating_add(RUST_PATH_SEPARATOR.len()))
        .map_err(|_| ExtractError::OutputLimit)?;
    path.push_str(RUST_PATH_SEPARATOR);
    path.push_str(component);
    Ok(())
}

fn rust_use_local_name(path: &str) -> Option<&str> {
    path.rsplit("::").next().filter(|name| !name.is_empty())
}

fn visit_python_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() == "import_from_statement" {
        visit_python_from_import(builder, node)
    } else {
        visit_python_plain_import(builder, node)
    }
}

fn visit_python_plain_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let mut cursor = node.walk();
    for imported in node.children_by_field_name("name", &mut cursor) {
        let Some(binding) = python_import_name(builder, imported)? else {
            continue;
        };
        let local_name = binding.alias.unwrap_or_else(|| {
            binding
                .name
                .split('.')
                .next()
                .unwrap_or(binding.name.as_str())
                .to_owned()
        });
        emit_import(
            builder,
            PolyglotImport {
                node,
                module_specifier: binding.name,
                kind: ImportBindingKind::Namespace,
                imported_name: "*".to_owned(),
                local_name,
                binding_node: binding.name_node,
            },
        )?;
    }
    Ok(())
}

fn visit_python_from_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(module_node) = node.child_by_field_name("module_name") else {
        return Ok(());
    };
    let raw_module = builder.context.owned_text(module_node)?;
    let package_prefix = python_package_relative_prefix(&raw_module);
    let module_specifier = python_module_specifier(&raw_module);
    if package_prefix.is_none() {
        emit_import_symbol_and_reference(builder, node, raw_module)?;
    }
    let mut cursor = node.walk();
    for imported in node.children_by_field_name("name", &mut cursor) {
        let Some(binding) = python_import_name(builder, imported)? else {
            continue;
        };
        emit_python_from_binding(
            builder,
            PythonFromBinding {
                binding,
                module_specifier: &module_specifier,
                package_prefix: package_prefix.as_deref(),
            },
        )?;
    }
    Ok(())
}

fn emit_python_from_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PythonFromBinding<'_, '_>,
) -> Result<(), ExtractError> {
    let PythonFromBinding {
        binding,
        module_specifier,
        package_prefix,
    } = input;
    let local_name = binding.alias.unwrap_or_else(|| binding.name.clone());
    if let Some(prefix) = package_prefix {
        if binding.name == "*" {
            return Ok(());
        }
        return emit_import(
            builder,
            PolyglotImport {
                node: binding.imported,
                module_specifier: format!("{prefix}{}", binding.name.replace('.', "/")),
                kind: ImportBindingKind::Namespace,
                imported_name: "*".to_owned(),
                local_name,
                binding_node: binding.name_node,
            },
        );
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Named,
        module_specifier: module_specifier.to_owned(),
        imported_name: binding.name.clone(),
        local_name,
        span: span_for(binding.name_node)?,
    })?;
    emit_node_reference(
        builder,
        PolyglotNodeReference {
            owner: None,
            node: binding.name_node,
            kind: ReferenceKind::References,
        },
    )
}

fn python_import_name<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    imported: Node<'tree>,
) -> Result<Option<PythonImportName<'tree>>, ExtractError> {
    let (name_node, alias_node) = if imported.kind() == "aliased_import" {
        (
            imported.child_by_field_name("name"),
            imported.child_by_field_name("alias"),
        )
    } else {
        (Some(imported), None)
    };
    let Some(name_node) = name_node else {
        return Ok(None);
    };
    Ok(Some(PythonImportName {
        imported,
        name_node,
        name: builder.context.owned_text(name_node)?,
        alias: alias_node
            .map(|alias| builder.context.owned_text(alias))
            .transpose()?,
    }))
}

fn visit_go_imports(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    for specifier in descendants_including_root(node) {
        if specifier.kind() != "import_spec" {
            continue;
        }
        let Some(path_node) = specifier.child_by_field_name("path") else {
            continue;
        };
        let module_specifier = builder.context.owned_unquoted_text(path_node)?;
        if super::specifier_safety::specifier_may_carry_credential(&module_specifier) {
            continue;
        }
        let local_name = match specifier.child_by_field_name("name") {
            Some(alias) => builder.context.owned_text(alias)?,
            None => module_specifier
                .rsplit('/')
                .next()
                .unwrap_or(module_specifier.as_str())
                .to_owned(),
        };
        emit_import(
            builder,
            PolyglotImport {
                node: specifier,
                module_specifier,
                kind: ImportBindingKind::Namespace,
                imported_name: "*".to_owned(),
                local_name,
                binding_node: path_node,
            },
        )?;
    }
    Ok(())
}

fn visit_go_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: GoTypeDeclaration<'_>,
) -> Result<(), ExtractError> {
    let node = declaration.node;
    let Some(type_node) = node.child_by_field_name("type") else {
        return Ok(());
    };
    let kind = if declaration.alias {
        SymbolKind::TypeAlias
    } else {
        match type_node.kind() {
            "struct_type" => SymbolKind::Struct,
            "interface_type" => SymbolKind::Interface,
            _ => SymbolKind::TypeAlias,
        }
    };
    let exported = node
        .child_by_field_name("name")
        .is_some_and(|name| go_exported(builder, name));
    if matches!(kind, SymbolKind::Struct | SymbolKind::Interface) {
        let name_node = node
            .child_by_field_name("name")
            .ok_or(ExtractError::InvalidSpan)?;
        let name = builder.context.owned_text(name_node)?;
        let owner = visit_named_container(
            builder,
            ContainerDeclaration {
                node,
                depth: declaration.depth,
                kind,
                exported,
                visibility: None,
            },
        )?;
        builder.owners.push(owner);
        builder.qualifiers.push(name);
        builder.visit(type_node, declaration.depth.saturating_add(1))?;
        builder.qualifiers.pop();
        builder.owners.pop();
        Ok(())
    } else {
        visit_leaf_declaration(
            builder,
            LeafDeclaration {
                node,
                depth: declaration.depth,
                kind,
                exported,
                visibility: None,
            },
        )
    }
}

fn visit_go_package(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = named_children(node).find(|child| child.kind() == "package_identifier")
    else {
        return Ok(());
    };
    let pending = PendingSymbol {
        kind: SymbolKind::Module,
        name: builder.context.owned_text(name_node)?,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol(pending).map(|_| ())
}

fn visit_go_method(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let receiver = go_receiver_owner(builder, node)?;
    let exported = node
        .child_by_field_name("name")
        .is_some_and(|name| go_exported(builder, name));
    visit_scoped_callable(
        builder,
        CallableScope {
            declaration: CallableDeclaration {
                node,
                depth,
                kind: SymbolKind::Method,
                exported,
                async_symbol: false,
                static_member: false,
                visibility: None,
            },
            owner: receiver.symbol,
            qualifier: receiver.name,
        },
    )
}

fn go_receiver_owner(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<ReceiverOwner, ExtractError> {
    let name = node
        .child_by_field_name("receiver")
        .and_then(|receiver| {
            descendants_including_root(receiver)
                .find(|candidate| candidate.kind() == "type_identifier")
        })
        .map(|name| builder.context.owned_text(name))
        .transpose()?;
    let symbol = name
        .as_deref()
        .and_then(|name| top_level_symbol(builder, name));
    Ok(ReceiverOwner { symbol, name })
}

fn emit_import(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PolyglotImport<'_>,
) -> Result<(), ExtractError> {
    emit_import_symbol_and_reference(builder, input.node, input.module_specifier.clone())?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: input.kind,
        module_specifier: input.module_specifier,
        imported_name: input.imported_name,
        local_name: input.local_name,
        span: span_for(input.binding_node)?,
    })
}

fn emit_import_symbol_and_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    module_name: String,
) -> Result<(), ExtractError> {
    let pending = PendingSymbol {
        kind: SymbolKind::Import,
        name: module_name.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: crate::SymbolExportFlags::new(false, false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    };
    builder.emit_symbol(pending)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: module_name,
            kind: ReferenceKind::Imports,
            node,
        },
    )
}

fn emit_node_reference(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PolyglotNodeReference<'_>,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(input.node)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: input.owner,
            name,
            kind: input.kind,
            node: input.node,
        },
    )
}

fn top_level_symbol(builder: &ExtractionBuilder<'_, '_>, name: &str) -> Option<SymbolId> {
    builder
        .facts
        .symbols
        .iter()
        .find(|symbol| {
            symbol.name == name
                && symbol.qualified_name == name
                && matches!(
                    symbol.kind,
                    SymbolKind::Struct
                        | SymbolKind::Class
                        | SymbolKind::Enum
                        | SymbolKind::Interface
                        | SymbolKind::Trait
                        | SymbolKind::TypeAlias
                )
        })
        .map(|symbol| symbol.id.clone())
}

fn python_module_specifier(raw: &str) -> String {
    let dots = raw
        .chars()
        .take_while(|character| *character == '.')
        .count();
    let suffix = raw[dots..].replace('.', "/");
    if dots == 0 {
        return suffix;
    }
    if dots == 1 {
        format!("./{suffix}")
    } else {
        format!("{}{suffix}", "../".repeat(dots.saturating_sub(1)))
    }
}

fn python_package_relative_prefix(raw: &str) -> Option<String> {
    let dots = raw
        .chars()
        .take_while(|character| *character == '.')
        .count();
    (dots > 0 && dots == raw.len()).then(|| {
        if dots == 1 {
            "./".to_owned()
        } else {
            "../".repeat(dots.saturating_sub(1))
        }
    })
}

fn python_exported(builder: &ExtractionBuilder<'_, '_>, name: Node<'_>) -> bool {
    builder.owners.is_empty() && !builder.context.text(name).trim().starts_with('_')
}

fn go_exported(builder: &ExtractionBuilder<'_, '_>, name: Node<'_>) -> bool {
    builder
        .context
        .text(name)
        .trim()
        .chars()
        .next()
        .is_some_and(char::is_uppercase)
}

fn rust_visibility(builder: &ExtractionBuilder<'_, '_>, node: Node<'_>) -> Option<Visibility> {
    let visibility = named_children(node)
        .find(|child| child.kind() == "visibility_modifier")
        .map(|node| builder.context.text(node).trim())?;
    match visibility {
        "pub" => Some(Visibility::Public),
        "pub(crate)" | "pub(super)" => Some(Visibility::Internal),
        _ => None,
    }
}

fn rust_async(node: Node<'_>) -> bool {
    named_children(node)
        .find(|child| child.kind() == "function_modifiers")
        .is_some_and(|modifiers| children(modifiers).any(|child| child.kind() == "async"))
}

fn is_rust_associated_callable(node: Node<'_>) -> bool {
    node.parent()
        .and_then(|parent| parent.parent())
        .is_some_and(|parent| matches!(parent.kind(), "impl_item" | "trait_item"))
}

fn is_python_class_member(node: Node<'_>) -> bool {
    node.parent()
        .and_then(|parent| {
            if parent.kind() == "decorated_definition" {
                parent.parent()
            } else {
                Some(parent)
            }
        })
        .and_then(|body| body.parent())
        .is_some_and(|parent| parent.kind() == "class_definition")
}
