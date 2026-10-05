//! Objective-C structural extraction.
//!
//! Objective-C is a strict superset of C, so the C subset (includes, functions,
//! structs, typedefs, globals, and macros) is delegated to the C family and both
//! languages share one contract. This module owns the Objective-C layer: classes
//! reopened by categories and `@implementation` blocks, protocols, methods named
//! by their full selector, `@property` names, and message sends named by receiver
//! and full selector so a call name equals the method name it targets.

use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, c_family, references,
    syntax::{children, descendants_including_root, named_children},
    with_root_scope,
};

/// Declarator nesting walked to find a property name (`(^handler)(int)` is four deep).
const MAXIMUM_PROPERTY_DECLARATOR_DEPTH: usize = 16;
/// Generic-parameter list tokens of one class header read for parameter names.
const MAXIMUM_GENERIC_PARAMETER_TOKENS: usize = 256;
/// Receivers that name the current object and are dropped from a message-send name.
const SELF_RECEIVERS: [&str; 2] = ["self", "super"];

/// The declaring node and name of one Objective-C class or protocol container.
#[derive(Clone, Copy)]
struct ContainerHead<'tree, 'name> {
    node: Node<'tree>,
    name_node: Node<'tree>,
    name: &'name str,
}

/// A container symbol whose members are visited with it as the owner.
struct OwnedMembers<'tree> {
    node: Node<'tree>,
    id: SymbolId,
    kind: SymbolKind,
    name: String,
    depth: usize,
}

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "class_interface" | "class_implementation" => visit_class(builder, node, depth)?,
        "protocol_declaration" => visit_protocol(builder, node, depth)?,
        "method_declaration" | "method_definition" => visit_method(builder, node, depth)?,
        "property_declaration" => visit_property(builder, node)?,
        "implementation_definition" => visit_implementation_member(builder, node, depth)?,
        // `@class Foo;`, `@protocol Foo;`, and `@synthesize` declare nothing new.
        "class_declaration" | "protocol_forward_declaration" | "property_implementation" => {}
        _ => return c_family::visit_declaration(builder, node, depth),
    }
    Ok(true)
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() == "message_expression" {
        return capture_message_send(builder, node);
    }
    c_family::capture_usage(builder, node)
}

/// An `@implementation` member: methods belong to the class, while C functions
/// and globals written between them keep file scope.
fn visit_implementation_member(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let member_depth = depth.saturating_add(1);
    for child in named_children(node) {
        if matches!(
            child.kind(),
            "method_definition" | "property_implementation"
        ) {
            builder.visit(child, member_depth)?;
        } else {
            with_root_scope(builder, |builder| builder.visit(child, member_depth))?;
        }
    }
    Ok(())
}

fn visit_class(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = first_identifier(node) else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let head = ContainerHead {
        node,
        name_node,
        name: &name,
    };
    let id = match reopened_class(builder, head)? {
        Some(id) => id,
        None => emit_container(builder, head, SymbolKind::Class)?,
    };
    if node.kind() == "class_interface" {
        capture_class_heritage(builder, head, &id)?;
    }
    visit_members(
        builder,
        OwnedMembers {
            node,
            id,
            kind: SymbolKind::Class,
            name,
            depth,
        },
    )
}

fn visit_protocol(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = first_identifier(node) else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let id = emit_container(
        builder,
        ContainerHead {
            node,
            name_node,
            name: &name,
        },
        SymbolKind::Protocol,
    )?;
    visit_members(
        builder,
        OwnedMembers {
            node,
            id,
            kind: SymbolKind::Protocol,
            name,
            depth,
        },
    )
}

/// Reuse the same-file class an `@interface`, category, class extension, or
/// `@implementation` already declared. The first occurrence keeps the span,
/// digest, and documentation; later blocks contribute their members through
/// containment. Only a primary `@implementation` turns a reused declaration
/// into the definition cross-file resolution prefers.
fn reopened_class(
    builder: &mut ExtractionBuilder<'_, '_>,
    head: ContainerHead<'_, '_>,
) -> Result<Option<SymbolId>, ExtractError> {
    let qualified_name = builder.qualified_name(head.name)?;
    let Some(existing) =
        builder.facts.symbols.iter_mut().find(|symbol| {
            symbol.kind == SymbolKind::Class && symbol.qualified_name == qualified_name
        })
    else {
        return Ok(None);
    };
    if defines_class(head.node) {
        existing.implementation.declaration_only = false;
    }
    Ok(Some(existing.id.clone()))
}

/// A primary `@implementation Foo` defines its class; an `@interface`, a class
/// extension `@interface Foo ()`, and any category only declare or augment it.
fn defines_class(node: Node<'_>) -> bool {
    node.kind() == "class_implementation"
        && node.child_by_field_name("category").is_none()
        && !has_token(node, "(")
}

/// Classes and protocols live in one global runtime namespace, so they are
/// exported; a class is a definition only where it is implemented.
fn emit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    head: ContainerHead<'_, '_>,
    kind: SymbolKind,
) -> Result<SymbolId, ExtractError> {
    let declaration_only = kind == SymbolKind::Class && !defines_class(head.node);
    let name = builder.context.copy_text(head.name)?;
    builder.emit_symbol(PendingSymbol {
        kind,
        name,
        span_node: head.node,
        structural_node: head.node,
        doc_anchor: head.node,
        body_node: (!declaration_only).then_some(head.node),
        declaration_only,
        signature: None,
        export: SymbolExportFlags::named(true),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })
}

fn visit_members(
    builder: &mut ExtractionBuilder<'_, '_>,
    members: OwnedMembers<'_>,
) -> Result<(), ExtractError> {
    builder.owners.push(members.id);
    builder.native_owner_kinds.push(members.kind);
    builder.native_visibilities.push(None);
    builder.qualifiers.push(members.name);
    let result = builder.visit_named_children(members.node, members.depth);
    builder.qualifiers.pop();
    builder.native_visibilities.pop();
    builder.native_owner_kinds.pop();
    builder.owners.pop();
    result
}

/// `: Superclass` extends; the protocol list implements.
fn capture_class_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    head: ContainerHead<'_, '_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let superclass = head.node.child_by_field_name("superclass");
    if let Some(target) = superclass {
        push_heritage(
            builder,
            Heritage {
                target,
                kind: ReferenceKind::Extends,
                owner,
            },
        )?;
    }
    let Some(protocols) = protocol_list(builder, head, superclass)? else {
        return Ok(());
    };
    for entry in named_children(protocols) {
        builder.context.ensure_active()?;
        if let Some(target) = named_children(entry)
            .find(|child| matches!(child.kind(), "type_identifier" | "identifier"))
        {
            push_heritage(
                builder,
                Heritage {
                    target,
                    kind: ReferenceKind::Implements,
                    owner,
                },
            )?;
        }
    }
    Ok(())
}

/// One superclass or protocol name written in a class header.
#[derive(Clone, Copy)]
struct Heritage<'tree, 'owner> {
    target: Node<'tree>,
    kind: ReferenceKind,
    owner: &'owner SymbolId,
}

/// The protocol list is the last `<…>` after the superclass, or after the
/// category parentheses. A list before them, or one attached directly to the
/// class name of a root class, is the class's lightweight-generic parameters.
/// A list naming pointer types, `id`, or one of the class's own generic
/// parameters is the superclass's type arguments (`: NSArray<NSString *>`,
/// `: Base<T>`). Neither is a protocol list.
fn protocol_list<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    head: ContainerHead<'tree, '_>,
    superclass: Option<Node<'tree>>,
) -> Result<Option<Node<'tree>>, ExtractError> {
    let boundary = superclass
        .or_else(|| head.node.child_by_field_name("category"))
        .map(|node| node.end_byte())
        .or_else(|| {
            children(head.node)
                .find(|child| child.kind() == ")")
                .map(|paren| paren.end_byte())
        });
    let (generics, after): (Vec<_>, Vec<_>) = named_children(head.node)
        .filter(|child| child.kind() == "parameterized_arguments")
        .partition(|list| boundary.is_some_and(|boundary| list.start_byte() < boundary));
    let Some(&list) = after.last() else {
        return Ok(None);
    };
    if boundary.is_none() && list.start_byte() == head.name_node.end_byte() {
        return Ok(None);
    }
    let parameters = generic_parameter_names(builder, &generics)?;
    let source = builder.context.source();
    for node in descendants_including_root(list) {
        builder.context.ensure_active()?;
        let type_argument = matches!(node.kind(), "abstract_pointer_declarator" | "id")
            || node.kind() == "type_identifier"
                && source
                    .get(node.start_byte()..node.end_byte())
                    .is_some_and(|name| parameters.contains(name));
        if type_argument {
            return Ok(None);
        }
    }
    Ok(Some(list))
}

/// The names the class's generic parameter entries bind (`T` in
/// `<T : NSObject *>`), never their constraints, for a bounded entry count.
fn generic_parameter_names<'source>(
    builder: &mut ExtractionBuilder<'source, '_>,
    lists: &[Node<'_>],
) -> Result<BTreeSet<&'source str>, ExtractError> {
    let source = builder.context.source();
    let mut names = BTreeSet::new();
    let mut constraint = false;
    for token in lists
        .iter()
        .flat_map(|list| children(*list))
        .take(MAXIMUM_GENERIC_PARAMETER_TOKENS)
    {
        builder.context.ensure_active()?;
        match token.kind() {
            ":" => constraint = true,
            "," => constraint = false,
            _ if token.is_named() && !constraint => {
                if let Some(name) = descendants_including_root(token)
                    .find(|node| node.kind() == "type_identifier")
                    .and_then(|name| source.get(name.start_byte()..name.end_byte()))
                {
                    names.insert(name);
                }
            }
            _ => {}
        }
    }
    Ok(names)
}

fn push_heritage(
    builder: &mut ExtractionBuilder<'_, '_>,
    heritage: Heritage<'_, '_>,
) -> Result<(), ExtractError> {
    let name = builder.context.owned_text(heritage.target)?;
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(heritage.owner.clone()),
            name,
            kind: heritage.kind,
            node: heritage.target,
        },
    )
}

fn visit_method(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(selector) = declared_selector(builder, node)? else {
        return builder.visit_named_children(node, depth);
    };
    let body = named_children(node).find(|child| child.kind() == "compound_statement");
    let id = builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Method,
        name: selector.clone(),
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: body,
        declaration_only: node.kind() == "method_declaration",
        signature: None,
        export: SymbolExportFlags::named(false),
        async_symbol: false,
        static_member: has_token(node, "+"),
        visibility: None,
    })?;
    let Some(body) = body else {
        return Ok(());
    };
    visit_members(
        builder,
        OwnedMembers {
            node: body,
            id,
            kind: SymbolKind::Method,
            name: selector,
            depth,
        },
    )
}

/// `- (T)greet` is `greet`; `- (T)a:(X)x b:(Y)y` is `a:b:`; an anonymous
/// keyword (`- (T)a:(X)x :(Y)y`) contributes a bare `:` (`a::`).
fn declared_selector(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut selector = SelectorBuilder::default();
    for child in named_children(node) {
        match child.kind() {
            "identifier" => selector.keyword(builder, child)?,
            "method_parameter" => selector.colon(),
            _ => {}
        }
    }
    selector.finish(builder)
}

/// The selector of a message send, built from its `method:` keywords and the
/// `:` tokens that follow them; arguments never contribute.
fn sent_selector(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut selector = SelectorBuilder::default();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if cursor.field_name() == Some("method") {
                selector.keyword(builder, child)?;
            } else if !child.is_named() && child.kind() == ":" {
                selector.colon();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    selector.finish(builder)
}

/// Accumulates selector keywords: `keyword` records the pending keyword and a
/// colon consumes it; with no colon at all the first keyword is a unary selector.
#[derive(Default)]
struct SelectorBuilder {
    unary: Option<String>,
    pending: Option<String>,
    parts: String,
    colons: usize,
}

impl SelectorBuilder {
    fn keyword(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        node: Node<'_>,
    ) -> Result<(), ExtractError> {
        let keyword = builder.context.owned_text(node)?;
        if keyword.is_empty() {
            // A recovered MISSING keyword names nothing; with no other
            // keyword the selector is absent rather than blank.
            return Ok(());
        }
        if self.unary.is_none() {
            self.unary = Some(keyword.clone());
        }
        self.pending = Some(keyword);
        Ok(())
    }

    fn colon(&mut self) {
        if let Some(keyword) = self.pending.take() {
            self.parts.push_str(&keyword);
        }
        self.parts.push(':');
        self.colons = self.colons.saturating_add(1);
    }

    fn finish(self, builder: &ExtractionBuilder<'_, '_>) -> Result<Option<String>, ExtractError> {
        if self.colons == 0 {
            return Ok(self.unary);
        }
        builder.context.copy_text(&self.parts).map(Some)
    }
}

/// `[self greet]` and `[super greet]` call `greet`; `[obj a:1 b:2]` calls
/// `obj.a:b:`; a nested-message or literal receiver is omitted.
fn capture_message_send(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(selector) = sent_selector(builder, node)? else {
        return Ok(());
    };
    let receiver = match node.child_by_field_name("receiver") {
        Some(receiver) => c_family::safe_call_target(builder, receiver)?
            .filter(|name| !SELF_RECEIVERS.contains(&name.as_str())),
        None => None,
    };
    let name = match receiver {
        Some(receiver) => qualified_send(builder, &receiver, &selector)?,
        None => selector,
    };
    references::push_reference(
        builder,
        PendingReference {
            owner: builder.owners.last().cloned(),
            name,
            kind: ReferenceKind::Calls,
            node,
        },
    )
}

fn qualified_send(
    builder: &ExtractionBuilder<'_, '_>,
    receiver: &str,
    selector: &str,
) -> Result<String, ExtractError> {
    let length = receiver
        .len()
        .checked_add(selector.len())
        .and_then(|length| length.checked_add(1))
        .ok_or(ExtractError::OutputLimit)?;
    builder.context.budget.ensure_string_length(length)?;
    let mut name = String::new();
    name.try_reserve(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    name.push_str(receiver);
    name.push('.');
    name.push_str(selector);
    Ok(name)
}

/// `@property (nonatomic, copy) NSString *name;` is the property `name`; the
/// attributes and the type are never names.
fn visit_property(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(name_node) = property_name(node) else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    builder.emit_symbol(PendingSymbol {
        kind: SymbolKind::Property,
        name,
        span_node: node,
        structural_node: node,
        doc_anchor: node,
        body_node: None,
        declaration_only: false,
        signature: None,
        export: SymbolExportFlags::named(false),
        async_symbol: false,
        static_member: false,
        visibility: None,
    })?;
    Ok(())
}

fn property_name(node: Node<'_>) -> Option<Node<'_>> {
    let declaration = named_children(node).find(|child| child.kind() == "struct_declaration")?;
    let mut current =
        named_children(declaration).find(|child| child.kind() == "struct_declarator")?;
    for _ in 0..MAXIMUM_PROPERTY_DECLARATOR_DEPTH {
        if current.kind() == "identifier" {
            return has_text(current).then_some(current);
        }
        current = current.child_by_field_name("declarator").or_else(|| {
            named_children(current)
                .find(|child| child.kind() == "identifier" || child.kind().ends_with("_declarator"))
        })?;
    }
    None
}

/// The container's name, absent when the parser recovered it as a zero-width
/// MISSING identifier: a nameless class or protocol would fail the validation
/// of the whole generation.
fn first_identifier(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node)
        .find(|child| child.kind() == "identifier")
        .filter(|name| has_text(*name))
}

/// Whether the node spans source text (a MISSING node is zero-width).
fn has_text(node: Node<'_>) -> bool {
    !node.is_missing() && node.start_byte() < node.end_byte()
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    children(node).any(|child| !child.is_named() && child.kind() == kind)
}
