//! Dart structural extraction.
//!
//! Dart's grammar places a callable's `function_body` beside its signature
//! instead of inside it, spells calls as an expression followed by `selector`
//! nodes, and wraps library directives in `import_or_export`. This family
//! attaches every body to its signature exactly once, names calls from their
//! selector chain, and records URI imports with their bindings. Dart privacy is
//! lexical: a leading `_` makes a declaration library-private.

use std::collections::BTreeSet;

mod call_receivers;
mod constructors;
mod scope_parameters;
pub(super) use call_receivers::current_call;
pub(super) use constructors::declaration_syntax;
pub(super) use scope_parameters::unshadowed_receiver_parameters;

use cartograph_domain::{
    ReferenceKind, SymbolId, SymbolKind, Visibility, callable_signature_is_literal_free,
};
use tree_sitter::Node;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, SymbolExportFlags};

use super::{
    ExtractionBuilder, NamedDeclaration, NodeRange, PendingReference, PendingSymbol, SymbolScope,
    in_symbol_scope, references, safe_assignment_signature,
    specifier_safety::specifier_may_carry_credential,
    syntax::{children, has_child_kind, named_child_of_kind, named_children, span_for},
    widen_symbol_span, with_root_scope,
};

/// Longest import/export URI retained as a module specifier.
const MAXIMUM_URI_BYTES: usize = 1024;
/// Modifier and type siblings inspected before a top-level identifier list.
const MAXIMUM_MODIFIER_STEPS: usize = 8;
/// Consecutive comments skipped between a signature and its body, or between
/// a callee and its arguments, before the two are treated as unrelated.
const MAXIMUM_COMMENT_RUN: usize = 64;
/// Constructor signatures wrapped in a method or member declaration.
const CONSTRUCTOR_SIGNATURE_KINDS: [&str; 4] = [
    "constructor_signature",
    "constant_constructor_signature",
    "factory_constructor_signature",
    "redirecting_factory_constructor_signature",
];
/// Binding name standing for every name a library exposes.
const WILDCARD: &str = "*";
/// Owners whose callables are methods and whose variables are fields.
const MEMBER_OWNER_KINDS: [SymbolKind; 2] = [SymbolKind::Class, SymbolKind::Enum];

/// Extract one Dart declaration, returning whether `node` was consumed.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if visit_type_declaration(builder, node, depth)? {
        return Ok(true);
    }
    visit_member_declaration(builder, node, depth)
}

/// Record the call or construction `node` performs, if any.
pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    constructors::capture_redirect(builder, node)?;
    let (target, kind) = match node.kind() {
        "selector" => (selector_call_target(builder, node)?, ReferenceKind::Calls),
        "new_expression" | "const_object_expression" => (
            construction_target(builder, node)?,
            ReferenceKind::Instantiates,
        ),
        _ => return Ok(()),
    };
    let Some(name) = target else {
        return Ok(());
    };
    let pending = PendingReference {
        owner: builder.owners.last().cloned(),
        name,
        kind,
        node,
    };
    references::push_reference(builder, pending)
}

/// The class a `new`/`const` expression constructs. `T.x(..)` is ambiguous
/// between a named constructor of `T` and type `x` behind import prefix `T`;
/// only a prefix this file actually declares joins the two (`pkg.Widget`).
fn construction_target(
    builder: &ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let Some(head) = named_child_of_kind(node, "type_identifier") else {
        return Ok(None);
    };
    let head_text = builder.context.text(head).trim();
    let prefixed_type = next_code_sibling(head)
        .filter(|segment| matches!(segment.kind(), "type_identifier" | "identifier"))
        .filter(|_| is_import_prefix(builder, head_text));
    match prefixed_type {
        Some(segment) => super::joined_signature(
            builder,
            super::JoinedSignature::dotted(head_text, builder.context.text(segment).trim()),
        )
        .map(Some),
        None => builder.context.copy_text(head_text).map(Some),
    }
}

/// Whether `name` is an `import .. as name` prefix declared earlier in the file
/// (Dart requires directives before declarations).
fn is_import_prefix(builder: &ExtractionBuilder<'_, '_>, name: &str) -> bool {
    builder
        .facts
        .import_bindings
        .iter()
        .any(|binding| binding.kind == ImportBindingKind::Namespace && binding.local_name == name)
}

/// Types, enum constants, aliases, and library directives.
fn visit_type_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "class_definition"
        | "mixin_declaration"
        | "extension_declaration"
        | "extension_type_declaration" => {
            visit_container(builder, Container::new(node, depth, SymbolKind::Class))?;
        }
        "enum_declaration" => {
            visit_container(builder, Container::new(node, depth, SymbolKind::Enum))?;
        }
        "enum_constant" => {
            if let Some(name) = node.child_by_field_name("name") {
                emit_leaf(
                    builder,
                    NamedDeclaration::new(node, name, SymbolKind::EnumMember),
                )?;
            }
        }
        "type_alias" => {
            if let Some(name) = typedef_name(node) {
                emit_leaf(
                    builder,
                    NamedDeclaration::new(node, name, SymbolKind::TypeAlias),
                )?;
            }
        }
        "import_or_export" => visit_directive(builder, node)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// The alias a `typedef` declares: the name just before the parameters of the
/// legacy `typedef int Compare(..)` form, else the leading name of
/// `typedef Name = ..`.
fn typedef_name(node: Node<'_>) -> Option<Node<'_>> {
    let legacy = named_child_of_kind(node, "formal_parameter_list")
        .and_then(|parameters| {
            std::iter::successors(parameters.prev_named_sibling(), Node::prev_named_sibling).find(
                |sibling| {
                    sibling.kind() != "type_parameters" && !sibling.kind().contains("comment")
                },
            )
        })
        .filter(|name| matches!(name.kind(), "type_identifier" | "identifier"));
    legacy.or_else(|| named_child_of_kind(node, "type_identifier"))
}

/// Callables, their paired bodies, fields, and top-level variables.
fn visit_member_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "function_signature" | "method_signature" => visit_signature(builder, node, depth)?,
        // A body paired with a signature is visited inside that callable.
        "function_body" if paired_signature(node).is_some() => {}
        "declaration" if in_member_scope(builder) => visit_member_variables(builder, node, depth)?,
        "initialized_identifier_list" | "static_final_declaration_list"
            if node
                .parent()
                .is_some_and(|parent| parent.kind() == "program") =>
        {
            visit_bindings(
                builder,
                Bindings {
                    list: node,
                    depth,
                    kind: top_level_binding_kind(node),
                },
            )?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Whether declarations here are members of a class-like owner.
fn in_member_scope(builder: &ExtractionBuilder<'_, '_>) -> bool {
    super::current_owner_kind_in(builder, &MEMBER_OWNER_KINDS)
}

/// The visibility a Dart identifier's spelling grants.
fn dart_visibility(name: &str) -> Visibility {
    if name.starts_with('_') {
        Visibility::Private
    } else {
        Visibility::Public
    }
}

/// How visible one declaration is outside its scope.
struct DartAccess {
    visibility: Option<Visibility>,
    exported: bool,
}

/// Library-level declarations and class members take their visibility from
/// their spelling, and only public library-level ones are exported; local
/// functions and variables have no visibility outside their body.
fn dart_access(
    builder: &ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    name: &str,
) -> DartAccess {
    let library_level = declaration
        .parent()
        .is_some_and(|parent| parent.kind() == "program");
    let visibility = (library_level || in_member_scope(builder)).then(|| dart_visibility(name));
    DartAccess {
        exported: library_level && visibility == Some(Visibility::Public),
        visibility,
    }
}

/// Emit a childless declaration with Dart privacy and library exports.
fn emit_leaf(
    builder: &mut ExtractionBuilder<'_, '_>,
    leaf: NamedDeclaration<'_>,
) -> Result<SymbolId, ExtractError> {
    let name = builder.context.owned_text(leaf.name)?;
    let access = dart_access(builder, leaf.node, &name);
    builder.emit_symbol(PendingSymbol {
        export: SymbolExportFlags::named(access.exported),
        visibility: access.visibility,
        ..PendingSymbol::plain(leaf.kind, name, leaf.node)
    })
}

/// A class-like declaration whose body holds members.
#[derive(Clone, Copy)]
struct Container<'tree> {
    node: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
}

impl<'tree> Container<'tree> {
    /// Bundle a class-like declaration with its depth and kind.
    const fn new(node: Node<'tree>, depth: usize, kind: SymbolKind) -> Self {
        Self { node, depth, kind }
    }
}

/// Emit a class-like declaration, its heritage, and its members.
fn visit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    container: Container<'_>,
) -> Result<(), ExtractError> {
    let Container { node, depth, kind } = container;
    let Some(name_node) = node
        .child_by_field_name("name")
        .or_else(|| named_child_of_kind(node, "identifier"))
    else {
        return builder.visit_named_children(node, depth);
    };
    let id = emit_leaf(builder, NamedDeclaration::new(node, name_node, kind))?;
    capture_heritage(builder, node, &id)?;
    let body = node.child_by_field_name("body").or_else(|| {
        named_children(node)
            .find(|child| matches!(child.kind(), "class_body" | "extension_body" | "enum_body"))
    });
    let name = builder.context.owned_text(name_node)?;
    in_symbol_scope(builder, SymbolScope { id, kind, name }, |builder| {
        constructors::emit_representation(builder, node)?;
        match body {
            Some(body) => builder.visit_named_children(body, depth.saturating_add(1)),
            None => Ok(()),
        }
    })
}

/// `extends` (Extends), `with` (Inherits), and `implements` (Implements).
fn capture_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for clause in named_children(node) {
        builder.context.ensure_active()?;
        match clause.kind() {
            "superclass" => {
                let base = dotted_type_names(builder, clause)?.into_iter().take(1);
                push_type_references(
                    builder,
                    owner,
                    base.map(|base| (base, ReferenceKind::Extends)),
                )?;
                if let Some(mixins) = named_child_of_kind(clause, "mixins") {
                    let mixins = dotted_type_names(builder, mixins)?;
                    push_type_references(
                        builder,
                        owner,
                        mixins
                            .into_iter()
                            .map(|mixin| (mixin, ReferenceKind::Inherits)),
                    )?;
                }
            }
            "interfaces" => {
                let interfaces = dotted_type_names(builder, clause)?;
                push_type_references(
                    builder,
                    owner,
                    interfaces
                        .into_iter()
                        .map(|interface| (interface, ReferenceKind::Implements)),
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// One nominal type a clause names, possibly library-prefixed (`pkg.Base`).
struct DottedType<'tree> {
    name: String,
    node: Node<'tree>,
}

/// The nominal types named directly in `clause`, joining a library prefix to
/// its type (`pkg.Base`) across comments and ignoring type arguments.
fn dotted_type_names<'tree>(
    builder: &ExtractionBuilder<'_, '_>,
    clause: Node<'tree>,
) -> Result<Vec<DottedType<'tree>>, ExtractError> {
    let mut types: Vec<DottedType<'tree>> = Vec::new();
    let mut continues_previous = false;
    for child in children(clause) {
        match child.kind() {
            "type_identifier" => {
                let segment = builder.context.text(child).trim();
                match types.last_mut().filter(|_| continues_previous) {
                    Some(previous) => {
                        previous.name = super::joined_signature(
                            builder,
                            super::JoinedSignature::dotted(&previous.name, segment),
                        )?;
                    }
                    None => types.push(DottedType {
                        name: builder.context.copy_text(segment)?,
                        node: child,
                    }),
                }
                continues_previous = false;
            }
            "." => continues_previous = true,
            kind if kind.contains("comment") => {}
            _ => continues_previous = false,
        }
    }
    Ok(types)
}

/// One heritage reference per listed type, all owned by `owner`.
fn push_type_references<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    owner: &SymbolId,
    targets: impl Iterator<Item = (DottedType<'tree>, ReferenceKind)>,
) -> Result<(), ExtractError> {
    for (target, kind) in targets {
        references::push_reference(
            builder,
            PendingReference {
                owner: Some(owner.clone()),
                name: target.name,
                kind,
                node: target.node,
            },
        )?;
    }
    Ok(())
}

/// The named signature inside a method/member wrapper, or the signature itself.
fn named_signature(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let inner = if matches!(node.kind(), "method_signature" | "declaration") {
        named_children(node).find(|child| {
            matches!(
                child.kind(),
                "function_signature" | "getter_signature" | "setter_signature"
            ) || CONSTRUCTOR_SIGNATURE_KINDS.contains(&child.kind())
        })?
    } else {
        node
    };
    if CONSTRUCTOR_SIGNATURE_KINDS.contains(&inner.kind()) {
        // The last identifier before parameters is the named constructor,
        // or the class for an unnamed one. Redirect targets follow parameters.
        return named_children(inner)
            .take_while(|child| child.kind() != "formal_parameter_list")
            .filter(|child| child.kind() == "identifier")
            .last()
            .map(|name| (inner, name));
    }
    inner.child_by_field_name("name").map(|name| (inner, name))
}

/// The nearest following named sibling that is not a comment.
fn next_code_sibling(node: Node<'_>) -> Option<Node<'_>> {
    std::iter::successors(node.next_named_sibling(), Node::next_named_sibling)
        .take(MAXIMUM_COMMENT_RUN.saturating_add(1))
        .find(|sibling| !sibling.kind().contains("comment"))
}

/// The nearest preceding named sibling that is not a comment.
pub(super) fn previous_code_sibling(node: Node<'_>) -> Option<Node<'_>> {
    std::iter::successors(node.prev_named_sibling(), Node::prev_named_sibling)
        .take(MAXIMUM_COMMENT_RUN.saturating_add(1))
        .find(|sibling| !sibling.kind().contains("comment"))
}

/// The `function_body` that follows `signature`, skipping interleaved comments.
fn paired_body(signature: Node<'_>) -> Option<Node<'_>> {
    next_code_sibling(signature).filter(|body| body.kind() == "function_body")
}

/// The callable signature that owns `body`, when its visit attaches it.
fn paired_signature(body: Node<'_>) -> Option<Node<'_>> {
    previous_code_sibling(body).filter(|signature| signature_owns_body(*signature))
}

/// Whether visiting `signature` emits a callable that consumes its sibling
/// body. Operators and members of an anonymous extension emit no symbol,
/// so their bodies stay with the enclosing scope.
fn signature_owns_body(signature: Node<'_>) -> bool {
    matches!(
        signature.kind(),
        "function_signature" | "method_signature" | "declaration"
    ) && signature_is_standalone(signature)
        && named_signature(signature).is_some()
        && !in_anonymous_extension(signature)
}

/// A signature nested in a `method_signature` or member `declaration` is
/// visited by that wrapper rather than on its own.
fn signature_is_standalone(signature: Node<'_>) -> bool {
    signature.kind() == "method_signature"
        || !signature
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "method_signature" | "declaration"))
}

/// Whether `member` sits in `extension on T { .. }`, which names no owner.
fn in_anonymous_extension(member: Node<'_>) -> bool {
    member
        .parent()
        .filter(|body| body.kind() == "extension_body")
        .and_then(|body| body.parent())
        .is_some_and(|extension| extension.child_by_field_name("name").is_none())
}

/// Emit a standalone named signature with its paired body.
fn visit_signature(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if !signature_is_standalone(node) {
        return Ok(());
    }
    let named = named_signature(node).filter(|_| signature_owns_body(node));
    let Some((inner, name_node)) = named else {
        // Anonymous-extension members still hold calls in the enclosing scope.
        return builder.visit_named_children(node, depth);
    };
    visit_callable(
        builder,
        Callable {
            signature: node,
            inner,
            name: name_node,
            body: paired_body(node),
            depth,
        },
    )
}

#[derive(Clone, Copy)]
/// A named callable signature and the body the grammar keeps beside it.
struct Callable<'tree> {
    /// The outermost method, function, or member declaration signature node.
    signature: Node<'tree>,
    /// The signature carrying the name, parameters, and return type.
    inner: Node<'tree>,
    name: Node<'tree>,
    body: Option<Node<'tree>>,
    depth: usize,
}

/// Emit a function or method and visit its body inside it.
fn visit_callable(
    builder: &mut ExtractionBuilder<'_, '_>,
    callable: Callable<'_>,
) -> Result<(), ExtractError> {
    let kind = if in_member_scope(builder) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let name = builder.context.owned_text(callable.name)?;
    let access = dart_access(builder, callable.signature, &name);
    let constructor = CONSTRUCTOR_SIGNATURE_KINDS.contains(&callable.inner.kind());
    let pending = PendingSymbol {
        body_node: callable.body.or_else(|| constructor_initializer(callable)),
        declaration_only: if constructor {
            has_child_kind(callable.signature, "external")
        } else {
            callable.body.is_none()
        },
        signature: callable_signature(builder, callable.inner)?,
        export: SymbolExportFlags::named(access.exported),
        async_symbol: callable
            .body
            .is_some_and(|body| has_child_kind(body, "async")),
        static_member: constructors::static_context(callable),
        visibility: access.visibility,
        // The body, not the signature, carries what makes two callables
        // structurally distinct.
        structural_node: callable.body.unwrap_or(callable.signature),
        ..PendingSymbol::plain(kind, name.clone(), callable.signature)
    };
    let id = builder.emit_symbol(pending)?;
    complete_constructor_evidence(builder, callable)?;
    if let Some(body) = callable.body {
        widen_symbol_span(
            builder,
            &id,
            NodeRange {
                first: callable.signature,
                last: body,
            },
        )?;
    }
    in_symbol_scope(builder, SymbolScope { id, kind, name }, |builder| {
        visit_callable_contents(builder, callable)
    })
}

/// Constructor initialization is executable even without a function body.
fn constructor_initializer(callable: Callable<'_>) -> Option<Node<'_>> {
    CONSTRUCTOR_SIGNATURE_KINDS
        .contains(&callable.inner.kind())
        .then(|| {
            named_children(callable.signature)
                .find(|child| matches!(child.kind(), "initializers" | "redirection"))
        })
        .flatten()
}

/// A constructor's identity and search evidence include its initializer list.
fn complete_constructor_evidence(
    builder: &mut ExtractionBuilder<'_, '_>,
    callable: Callable<'_>,
) -> Result<(), ExtractError> {
    let Some(initializer) = constructor_initializer(callable) else {
        return Ok(());
    };
    let digest = super::syntax::structural_digest_for_nodes(
        std::iter::once(callable.signature).chain(callable.body),
        builder.context.source,
        builder.context.cancelled,
    )?;
    let search = super::syntax::body_search_text_for_nodes(
        std::iter::once(initializer).chain(callable.body),
        builder.context.source,
        builder.context.cancelled,
    )?;
    builder
        .context
        .budget
        .reserve_additional_string(&search.text)?;
    if let Some(symbol) = builder.facts.symbols.last_mut() {
        symbol.structural_digest = digest;
        symbol.body_search_text = search.text;
        symbol.body_search_truncated = search.truncated;
    }
    Ok(())
}

/// Constructor initializers and callable bodies belong to their declaration.
fn visit_callable_contents(
    builder: &mut ExtractionBuilder<'_, '_>,
    callable: Callable<'_>,
) -> Result<(), ExtractError> {
    let depth = callable.depth.saturating_add(1);
    if CONSTRUCTOR_SIGNATURE_KINDS.contains(&callable.inner.kind()) {
        builder.visit_named_children(callable.inner, depth)?;
    }
    for initializer in named_children(callable.signature)
        .filter(|child| matches!(child.kind(), "initializers" | "redirection"))
    {
        constructors::capture_redirect(builder, initializer)?;
        builder.visit_named_children(initializer, depth)?;
    }
    match callable.body {
        Some(body) => builder.visit_named_children(body, depth),
        None => Ok(()),
    }
}

/// `<return type> <parameters>` when both are literal-free, as in v1.
fn callable_signature(
    builder: &ExtractionBuilder<'_, '_>,
    inner: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let name = inner.child_by_field_name("name");
    let return_type = if CONSTRUCTOR_SIGNATURE_KINDS.contains(&inner.kind()) {
        ""
    } else {
        type_text(
            builder,
            named_children(inner).take_while(|child| Some(*child) != name),
        )
    };
    let parameters = named_child_of_kind(inner, "formal_parameter_list")
        .map_or("", |parameters| builder.context.text(parameters).trim());
    bounded_signature(
        builder,
        super::JoinedSignature::words(return_type, parameters),
    )
}

/// The source text spanning the type nodes among `nodes`, empty when none.
fn type_text<'source, 'tree>(
    builder: &ExtractionBuilder<'source, '_>,
    nodes: impl Iterator<Item = Node<'tree>>,
) -> &'source str {
    let mut types = nodes.filter(|node| {
        matches!(
            node.kind(),
            "type_identifier" | "type_arguments" | "void_type" | "function_type" | "nullable_type"
        )
    });
    let Some(first) = types.next() else {
        return "";
    };
    let (start, end) = types.fold(
        (first.start_byte(), first.end_byte()),
        |(start, end), node| (start.min(node.start_byte()), end.max(node.end_byte())),
    );
    builder
        .context
        .snapshot
        .source()
        .get(start..end)
        .unwrap_or_default()
}

/// Join two signature halves, keeping the result only when it is bounded and
/// literal-free.
fn bounded_signature(
    builder: &ExtractionBuilder<'_, '_>,
    parts: super::JoinedSignature<'_>,
) -> Result<Option<String>, ExtractError> {
    let length = parts.left.len().saturating_add(parts.right.len());
    if length > super::MAX_SAFE_SIGNATURE_BYTES {
        return Ok(None);
    }
    let signature = match (parts.left.is_empty(), parts.right.is_empty()) {
        (true, true) => return Ok(None),
        (true, false) => builder.context.copy_text(parts.right)?,
        (false, true) => builder.context.copy_text(parts.left)?,
        (false, false) => super::joined_signature(builder, parts)?,
    };
    let commented = signature.contains("/*") || signature.contains("//");
    Ok((!commented && callable_signature_is_literal_free(&signature)).then_some(signature))
}

/// A member `declaration`: a callable, constructor, or field list.
fn visit_member_variables(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    if let Some((signature, name)) = named_signature(node) {
        return visit_callable(
            builder,
            Callable {
                signature: node,
                inner: signature,
                name,
                body: None,
                depth,
            },
        );
    }
    let Some(list) = named_children(node).find(|child| {
        matches!(
            child.kind(),
            "initialized_identifier_list" | "static_final_declaration_list" | "identifier_list"
        )
    }) else {
        return builder.visit_named_children(node, depth);
    };
    visit_bindings(
        builder,
        Bindings {
            list,
            depth,
            kind: SymbolKind::Field,
        },
    )
}

/// Variables declared together in one identifier list.
#[derive(Clone, Copy)]
struct Bindings<'tree> {
    list: Node<'tree>,
    depth: usize,
    kind: SymbolKind,
}

/// Emit every variable of one identifier list.
fn visit_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    bindings: Bindings<'_>,
) -> Result<(), ExtractError> {
    let declared_type = builder
        .context
        .copy_text(type_text(builder, declaration_modifiers(bindings.list)))?;
    let static_member = bindings
        .list
        .parent()
        .is_some_and(|parent| parent.kind() == "declaration" && has_child_kind(parent, "static"));
    for binding in named_children(bindings.list) {
        builder.context.ensure_active()?;
        let name_node = if binding.kind() == "identifier" {
            Some(binding)
        } else {
            named_child_of_kind(binding, "identifier")
        };
        let Some(name_node) = name_node else {
            continue;
        };
        visit_binding(
            builder,
            Binding {
                declarator: binding,
                name: name_node,
                declared_type: &declared_type,
                static_member,
                group: bindings,
            },
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
/// One variable of an identifier list with the facts shared by its list.
struct Binding<'tree, 'text> {
    declarator: Node<'tree>,
    name: Node<'tree>,
    declared_type: &'text str,
    static_member: bool,
    group: Bindings<'tree>,
}

/// Emit one variable and visit its initializer inside it.
fn visit_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: Binding<'_, '_>,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(input.name)?;
    let initializer = single_initializer(input.declarator, input.name);
    let signature = if input.group.kind == SymbolKind::Field {
        bounded_signature(
            builder,
            super::JoinedSignature::words(input.declared_type, &name),
        )?
    } else {
        initializer
            .map(|value| safe_assignment_signature(builder, value))
            .transpose()?
            .flatten()
    };
    let access = dart_access(builder, input.group.list, &name);
    let id = builder.emit_symbol(PendingSymbol {
        signature,
        export: SymbolExportFlags::named(access.exported),
        static_member: input.static_member,
        visibility: access.visibility,
        ..PendingSymbol::plain(input.group.kind, name.clone(), input.declarator)
    })?;
    let scope = SymbolScope {
        id,
        kind: input.group.kind,
        name,
    };
    in_symbol_scope(builder, scope, |builder| {
        for child in named_children(input.declarator).filter(|child| *child != input.name) {
            builder.visit(child, input.group.depth.saturating_add(1))?;
        }
        Ok(())
    })
}

/// The initializer of `name = value` when it is one expression node.
fn single_initializer<'tree>(binding: Node<'tree>, name: Node<'tree>) -> Option<Node<'tree>> {
    let mut values = named_children(binding).filter(|child| *child != name);
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

/// The modifier and type nodes that precede a top-level identifier list.
///
/// Comments interleaved with the modifiers are stepped over (still within the
/// step bound) so `const /* note */ answer` keeps its `const`.
fn declaration_modifiers(list: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(list.prev_named_sibling(), Node::prev_named_sibling)
        .take(MAXIMUM_MODIFIER_STEPS)
        .take_while(|node| {
            node.kind().contains("comment")
                || matches!(
                    node.kind(),
                    "const_builtin"
                        | "final_builtin"
                        | "type_identifier"
                        | "type_arguments"
                        | "inferred_type"
                        | "nullable_type"
                        | "function_type"
                )
        })
}

/// `const` top-level variables are constants; the rest are variables.
fn top_level_binding_kind(list: Node<'_>) -> SymbolKind {
    if declaration_modifiers(list).any(|modifier| modifier.kind() == "const_builtin") {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    }
}

/// The callee named by a `selector` that applies arguments, following v1:
/// `f(..)` names `f`; `r.m(..)` names `r.m`; any other receiver names `m`.
fn selector_call_target(
    builder: &mut ExtractionBuilder<'_, '_>,
    selector: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    if named_child_of_kind(selector, "argument_part").is_none() {
        return Ok(None);
    }
    let Some(previous) = previous_code_sibling(selector) else {
        return Ok(None);
    };
    let accessor = match previous.kind() {
        "identifier" => return builder.context.owned_text(previous).map(Some),
        "selector" => named_children(previous).find(|child| is_assignable_selector(child.kind())),
        kind if is_assignable_selector(kind) => Some(previous),
        _ => None,
    };
    let Some(member) = accessor.and_then(|accessor| named_child_of_kind(accessor, "identifier"))
    else {
        return Ok(None);
    };
    let member = builder.context.owned_text(member)?;
    let receiver = previous_code_sibling(previous)
        .filter(|receiver| previous.kind() == "selector" && receiver.kind() == "identifier");
    match receiver {
        Some(receiver) => {
            let receiver = builder.context.owned_text(receiver)?;
            super::joined_signature(builder, super::JoinedSignature::dotted(&receiver, &member))
                .map(Some)
        }
        None => Ok(Some(member)),
    }
}

/// Whether `kind` is a `.member` or `?.member` accessor.
fn is_assignable_selector(kind: &str) -> bool {
    matches!(
        kind,
        "unconditional_assignable_selector" | "conditional_assignable_selector"
    )
}

/// One `import`/`export` directive and the URI it names.
struct Directive<'tree> {
    node: Node<'tree>,
    /// The import specification or export node holding the URI and combinators.
    specification: Node<'tree>,
    uri: Node<'tree>,
    module: String,
}

/// Emit an `import`/`export` directive with its bindings.
fn visit_directive(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let (specification, export) = match named_children(node).next() {
        Some(child) if child.kind() == "library_import" => {
            (named_child_of_kind(child, "import_specification"), false)
        }
        Some(child) if child.kind() == "library_export" => (Some(child), true),
        _ => (None, false),
    };
    let Some(specification) = specification else {
        return Ok(());
    };
    let Some(uri) = named_child_of_kind(specification, "configurable_uri")
        .and_then(|uri| named_child_of_kind(uri, "uri"))
    else {
        return Ok(());
    };
    let Some(module) = directive_module(builder, uri)? else {
        return Ok(());
    };
    let directive = Directive {
        node,
        specification,
        uri,
        module,
    };
    emit_directive_symbol(builder, &directive)?;
    if export {
        return emit_export_bindings(builder, &directive);
    }
    let prefix = named_child_of_kind(specification, "identifier")
        .map(|prefix| builder.context.owned_text(prefix))
        .transpose()?;
    emit_binding(
        builder,
        &directive,
        DirectiveBinding {
            kind: ImportBindingKind::Namespace,
            imported: WILDCARD.to_owned(),
            local: prefix.unwrap_or_else(|| WILDCARD.to_owned()),
        },
    )
}

/// How one directive binds a name from its module.
struct DirectiveBinding {
    kind: ImportBindingKind,
    imported: String,
    local: String,
}

/// Emit one import binding for a directive.
fn emit_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    directive: &Directive<'_>,
    binding: DirectiveBinding,
) -> Result<(), ExtractError> {
    builder.emit_import_binding(ExtractedImportBinding {
        kind: binding.kind,
        module_specifier: builder.context.copy_text(&directive.module)?,
        imported_name: binding.imported,
        local_name: binding.local,
        span: span_for(directive.uri)?,
    })
}

/// `export 'a.dart' show X, Y;` re-exports exactly the shown names that every
/// `show` clause lists and no `hide` clause removes; any other export
/// (including one that only hides names) re-exports the library.
fn emit_export_bindings(
    builder: &mut ExtractionBuilder<'_, '_>,
    directive: &Directive<'_>,
) -> Result<(), ExtractError> {
    let Some(shown) = exported_names(builder, directive.specification)? else {
        return emit_binding(
            builder,
            directive,
            DirectiveBinding {
                kind: ImportBindingKind::ReExportAll,
                imported: WILDCARD.to_owned(),
                local: WILDCARD.to_owned(),
            },
        );
    };
    for name in shown {
        builder.context.ensure_active()?;
        emit_binding(
            builder,
            directive,
            DirectiveBinding {
                kind: ImportBindingKind::ReExportNamed,
                imported: name.clone(),
                local: name,
            },
        )?;
    }
    Ok(())
}

/// The names an export's combinators admit, in source order, or `None` when
/// no `show` clause restricts it.
fn exported_names(
    builder: &mut ExtractionBuilder<'_, '_>,
    export: Node<'_>,
) -> Result<Option<Vec<String>>, ExtractError> {
    let mut shown: Option<Vec<String>> = None;
    let mut hidden = BTreeSet::new();
    for combinator in named_children(export).filter(|child| child.kind() == "combinator") {
        builder.context.ensure_active()?;
        let names = named_children(combinator)
            .filter(|name| name.kind() == "identifier")
            .map(|name| builder.context.copy_text(builder.context.text(name).trim()))
            .collect::<Result<Vec<_>, _>>()?;
        if !has_child_kind(combinator, "show") {
            hidden.extend(names);
            continue;
        }
        shown = Some(match shown {
            Some(previous) => {
                let admitted = names.into_iter().collect::<BTreeSet<_>>();
                previous
                    .into_iter()
                    .filter(|name| admitted.contains(name))
                    .collect()
            }
            None => names,
        });
    }
    Ok(shown.map(|names| {
        names
            .into_iter()
            .filter(|name| !hidden.contains(name))
            .collect()
    }))
}

/// The unquoted URI, rejecting interpolated, multi-line, or oversized text.
fn directive_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    uri: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let raw = builder.context.text(uri).trim();
    let unquoted = raw.trim_matches(|character| matches!(character, '\'' | '"'));
    if unquoted.is_empty()
        || unquoted.len() > MAXIMUM_URI_BYTES
        || unquoted.contains(['$', '\'', '"', '\n', '\r', '\0'])
        || specifier_may_carry_credential(unquoted)
    {
        return Ok(None);
    }
    builder.context.copy_text(unquoted).map(Some)
}

/// The root-scoped import symbol and its module reference.
fn emit_directive_symbol(
    builder: &mut ExtractionBuilder<'_, '_>,
    directive: &Directive<'_>,
) -> Result<(), ExtractError> {
    let name = directive.module.clone();
    with_root_scope(builder, |builder| {
        builder.emit_symbol(PendingSymbol::plain(
            SymbolKind::Import,
            name,
            directive.node,
        ))
    })?;
    references::push_reference(
        builder,
        PendingReference {
            owner: None,
            name: directive.module.clone(),
            kind: ReferenceKind::Imports,
            node: directive.uri,
        },
    )
}
