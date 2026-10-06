//! Ruby structural extraction.
//!
//! Restores the v1 Ruby extractor's observable facts on the native walker:
//! `require`/`require_relative` loads become imports, `attr_*` and
//! `class_attribute` macros declare fields, constant assignments declare
//! constants, statement-level bare identifiers that are not local variables are
//! calls, receiver calls keep their receiver (`Factory.run`), and
//! `private`/`protected`/`public` set the visibility of instance methods.

mod locals;
mod state;

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, Visibility};
use tree_sitter::Node;

use crate::{ExtractError, ImportBindingKind, SymbolExportFlags};

use self::locals::{LocalScope, ScopeKind, bind_targets, within_scope};
use self::state::MethodRestriction;
pub(super) use self::state::RubyState;
use super::{
    ExtractionBuilder, PendingReference, references,
    script_support::{
        LoadImport, OwnerScope, bounded_name, bounded_reference_name, bounded_text,
        emit_load_import, is_relative_specifier, literal_free_assignment_signature,
        literal_free_node_signature, plain_symbol, with_owner,
    },
    syntax::{named_children, starts_uppercase},
    with_root_scope,
};

/// Class macros that synthesize accessor fields from their symbol arguments.
const ACCESSOR_MACROS: [&str; 4] = [
    "attr_reader",
    "attr_writer",
    "attr_accessor",
    "class_attribute",
];
/// Statement containers whose direct identifier children are bare method calls.
const BARE_CALL_PARENTS: [&str; 9] = [
    "body_statement",
    "block_body",
    "then",
    "else",
    "do",
    "begin",
    "rescue",
    "ensure",
    "when",
];
/// Keywords and pseudo-variables that parse as identifiers but never call a method.
const BARE_CALL_SKIP_NAMES: [&str; 8] = [
    "true", "false", "nil", "self", "super", "__FILE__", "__LINE__", "__dir__",
];
/// Receivers that denote the current object and add no qualification to a call.
const SELF_RECEIVERS: [&str; 2] = ["self", "super"];
/// Receivers whose source text is a stable, literal-free name.
const NAMED_RECEIVERS: [&str; 5] = [
    "identifier",
    "constant",
    "instance_variable",
    "class_variable",
    "global_variable",
];
/// Statement lists in which a bare visibility keyword opens a section: class,
/// module, and singleton-class bodies, and block bodies, which isolate theirs.
const SECTION_BODIES: [&str; 2] = ["body_statement", "block_body"];
/// Maximum nesting followed through constant paths and modifier wrappers.
const MAX_PATH_DEPTH: usize = 16;
/// Restricted method definitions updated between cancellation checks.
const RESTRICTION_CANCELLATION_INTERVAL: usize = 256;
/// Instance methods Ruby makes private without any visibility modifier.
const IMPLICITLY_PRIVATE_METHODS: [&str; 5] = [
    "initialize",
    "initialize_copy",
    "initialize_clone",
    "initialize_dup",
    "respond_to_missing?",
];
/// Constructs that open a nested local scope seeing the enclosing locals.
const BLOCK_KINDS: [&str; 3] = ["block", "do_block", "lambda"];
/// Maximum nested receiver calls folded into one call name.
const MAX_RECEIVER_DEPTH: usize = 16;

pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "program" if node.parent().is_none() => {
            let scope = LocalScope {
                kind: ScopeKind::Fresh,
                parameters: None,
            };
            within_scope(builder, scope, |builder| {
                builder.visit_named_children(node, depth)
            })?;
            apply_restrictions(builder)?;
            Ok(true)
        }
        "module" | "class" => visit_container(builder, node, depth),
        "singleton_class" => visit_singleton_class(builder, node, depth).map(|()| true),
        "method" | "singleton_method" => visit_method(
            builder,
            MethodVisit {
                node,
                depth,
                explicit: None,
            },
        ),
        kind if BLOCK_KINDS.contains(&kind) => visit_block(builder, node, depth).map(|()| true),
        "assignment" => visit_assignment(builder, node, depth),
        "operator_assignment" | "exception_variable" | "for" | "in_clause" => {
            bind_introduced_locals(builder, node)?;
            Ok(false)
        }
        "match_pattern" | "test_pattern" => visit_pattern_match(builder, node, depth),
        "call" => visit_declaring_call(builder, node, depth),
        "identifier" => Ok(apply_section_modifier(builder, node)),
        _ => Ok(false),
    }
}

pub(super) fn capture_usage(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    match node.kind() {
        "call" => capture_call(builder, node),
        "identifier" => capture_bare_call(builder, node),
        _ => Ok(()),
    }
}

fn visit_container(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let kind = if node.kind() == "class" {
        SymbolKind::Class
    } else {
        SymbolKind::Module
    };
    let Some(name_node) = node
        .child_by_field_name("name")
        .filter(|name| is_constant_path(*name, 0))
    else {
        return Ok(false);
    };
    let Some(name) = bounded_name(builder, name_node)? else {
        return Ok(false);
    };
    let mut pending = plain_symbol(kind, name.clone(), node);
    pending.body_node = Some(node);
    pending.export = SymbolExportFlags::named(builder.owners.is_empty());
    let id = builder.emit_symbol(pending)?;
    if let Some(superclass) = node.child_by_field_name("superclass") {
        capture_superclass(builder, superclass, &id)?;
        // A computed superclass (`< Struct.new(:a)`) runs in the enclosing scope.
        builder.visit(superclass, depth.saturating_add(1))?;
    }
    let scope = OwnerScope {
        id: &id,
        kind,
        name: &name,
    };
    with_owner(builder, scope, |builder| {
        with_singleton_context(builder, SingletonContext::Instance, |builder| {
            visit_fresh_body(builder, node.child_by_field_name("body"), depth)
        })
    })?;
    Ok(true)
}

/// Visit a class, module, or singleton-class body in its own local scope.
fn visit_fresh_body(
    builder: &mut ExtractionBuilder<'_, '_>,
    body: Option<Node<'_>>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(body) = body else {
        return Ok(());
    };
    let scope = LocalScope {
        kind: ScopeKind::Fresh,
        parameters: None,
    };
    within_scope(builder, scope, |builder| {
        builder.visit(body, depth.saturating_add(1))
    })
}

fn capture_superclass(
    builder: &mut ExtractionBuilder<'_, '_>,
    superclass: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let Some(target) = named_children(superclass).find(|child| is_constant_path(*child, 0)) else {
        return Ok(());
    };
    let Some(name) = bounded_name(builder, target)? else {
        return Ok(());
    };
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner.clone()),
            name,
            kind: ReferenceKind::Inherits,
            node: target,
        },
    )
}

/// `class << self` declares singleton methods on the enclosing class with an
/// independent visibility section that must not leak into the class body.
fn visit_singleton_class(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(receiver) = node.child_by_field_name("value") else {
        return Ok(());
    };
    builder.visit(receiver, depth.saturating_add(1))?;
    if !enclosing_type_receiver(builder, receiver) {
        return Ok(());
    }
    with_isolated_section(builder, |builder| {
        with_singleton_context(builder, SingletonContext::Singleton, |builder| {
            visit_fresh_body(builder, node.child_by_field_name("body"), depth)
        })
    })
}

/// Whether the definition scope being entered is a `class << self` body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SingletonContext {
    Instance,
    Singleton,
}

/// Run `action` with `context` as the innermost definition scope's singleton
/// context, restoring the enclosing one afterwards.
fn with_singleton_context(
    builder: &mut ExtractionBuilder<'_, '_>,
    context: SingletonContext,
    action: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    let saved = builder.script.ruby.singleton_body;
    builder.script.ruby.singleton_body = context == SingletonContext::Singleton;
    let result = action(builder);
    builder.script.ruby.singleton_body = saved;
    result
}

/// A block body (`class_methods do ... end`, `included do ... end`) keeps its
/// own visibility section: `private` inside it governs only the methods that
/// follow it in that block, as v1's sibling scoping did, and never leaks into
/// the enclosing class or module body.
fn visit_block(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let scope = LocalScope {
        kind: ScopeKind::Nested,
        parameters: node.child_by_field_name("parameters"),
    };
    with_isolated_section(builder, |builder| {
        within_scope(builder, scope, |builder| {
            builder.visit_named_children(node, depth)
        })
    })
}

/// Run `action` starting from the default (public) section and restore the
/// enclosing section afterwards, whatever sections `action` opened.
fn with_isolated_section(
    builder: &mut ExtractionBuilder<'_, '_>,
    action: impl FnOnce(&mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError>,
) -> Result<(), ExtractError> {
    let saved = builder.native_visibilities.last().copied().flatten();
    set_section(builder, None);
    let result = action(builder);
    set_section(builder, saved);
    result
}

#[derive(Clone, Copy)]
struct MethodVisit<'tree> {
    node: Node<'tree>,
    depth: usize,
    explicit: Option<Visibility>,
}

fn visit_method(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: MethodVisit<'_>,
) -> Result<bool, ExtractError> {
    if input.node.kind() != "singleton_method" {
        return emit_method(builder, input);
    }
    let Some(receiver) = input.node.child_by_field_name("object") else {
        return Ok(true);
    };
    builder.visit(receiver, input.depth.saturating_add(1))?;
    if enclosing_type_receiver(builder, receiver) {
        emit_method(builder, input)
    } else {
        // A bare file-level Method records the definition without a type owner.
        with_root_scope(builder, |builder| emit_method(builder, input))
    }
}

/// Only self or the literal enclosing type name establishes a member owner.
fn enclosing_type_receiver(builder: &ExtractionBuilder<'_, '_>, receiver: Node<'_>) -> bool {
    let name = builder.context.text(receiver);
    inside_type(builder)
        && ((receiver.kind() == "self" && name == "self")
            || (is_constant_path(receiver, 0)
                && builder
                    .qualifiers
                    .last()
                    .is_some_and(|enclosing| name == enclosing)))
}

fn emit_method(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: MethodVisit<'_>,
) -> Result<bool, ExtractError> {
    let node = input.node;
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(false);
    };
    let Some(name) = bounded_name(builder, name_node)? else {
        return Ok(false);
    };
    let singleton = node.kind() == "singleton_method";
    let kind = if singleton || inside_type(builder) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    };
    let parameters = node.child_by_field_name("parameters");
    let signature = match parameters {
        Some(parameters) => {
            literal_free_node_signature(builder, parameters, builder.context.text(parameters))?
        }
        None => None,
    };
    let mut pending = plain_symbol(kind, name.clone(), node);
    pending.body_node = Some(node);
    pending.signature = signature;
    pending.static_member = singleton || builder.script.ruby.singleton_body;
    let static_member = pending.static_member;
    pending.visibility = method_visibility(
        builder,
        MethodVisibility {
            kind,
            singleton,
            name: &name,
            explicit: input.explicit,
        },
    );
    pending.export =
        SymbolExportFlags::named(kind == SymbolKind::Function && builder.owners.is_empty());
    let id = builder.emit_symbol(pending)?;
    if kind == SymbolKind::Method {
        record_emitted_method(builder, static_member);
    }
    let scope = OwnerScope {
        id: &id,
        kind,
        name: &name,
    };
    let locals = LocalScope {
        kind: ScopeKind::Fresh,
        parameters,
    };
    with_owner(builder, scope, |builder| {
        within_scope(builder, locals, |builder| {
            with_singleton_context(builder, SingletonContext::Instance, |builder| {
                visit_method_children(builder, input)
            })
        })
    })?;
    Ok(true)
}

/// The singleton receiver ran before entering this method's scope.
fn visit_method_children(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: MethodVisit<'_>,
) -> Result<(), ExtractError> {
    let receiver = input.node.child_by_field_name("object");
    for child in named_children(input.node).filter(|child| Some(*child) != receiver) {
        builder.visit(child, input.depth.saturating_add(1))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct MethodVisibility<'name> {
    kind: SymbolKind,
    singleton: bool,
    name: &'name str,
    explicit: Option<Visibility>,
}

/// Instance methods take an explicit wrapper (`public def initialize`), then
/// Ruby's implicit privacy, then the current section, defaulting to public;
/// sections never apply to `def self.x`.
fn method_visibility(
    builder: &ExtractionBuilder<'_, '_>,
    input: MethodVisibility<'_>,
) -> Option<Visibility> {
    if input.kind != SymbolKind::Method {
        return None;
    }
    if input.singleton {
        return Some(Visibility::Public);
    }
    if let Some(explicit) = input.explicit {
        return Some(explicit);
    }
    if IMPLICITLY_PRIVATE_METHODS.contains(&input.name) {
        return Some(Visibility::Private);
    }
    Some(current_section(builder))
}

fn visit_assignment(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let Some(left) = node.child_by_field_name("left") else {
        return Ok(false);
    };
    bind_targets(builder, left, 0)?;
    let kind = match left.kind() {
        "constant" => SymbolKind::Constant,
        "identifier" if builder.owners.is_empty() => SymbolKind::Variable,
        _ => return Ok(false),
    };
    let Some(name) = bounded_name(builder, left)? else {
        return Ok(false);
    };
    let right = node.child_by_field_name("right");
    let signature = match right {
        Some(value) => literal_free_assignment_signature(builder, value)?,
        None => None,
    };
    let chained = right.is_some_and(is_chained_assignment);
    let mut pending = plain_symbol(kind, name.clone(), node);
    if chained {
        // `A = B = 1`: `B` and its value are analysed as their own symbol, so
        // `A` is analysed through its target alone; a long chain is then
        // linear rather than re-analysed once per link.
        pending.structural_node = left;
    }
    pending.signature = signature;
    pending.export =
        SymbolExportFlags::named(kind == SymbolKind::Constant && builder.owners.is_empty());
    let id = builder.emit_symbol(pending)?;
    match right {
        // `A = B = 1` and `A = (B = 1)` declare `B` in the same scope as `A`.
        Some(value) if chained => {
            builder.visit(value, depth.saturating_add(1))?;
        }
        Some(value) => {
            let scope = OwnerScope {
                id: &id,
                kind,
                name: &name,
            };
            with_owner(builder, scope, |builder| {
                builder.visit(value, depth.saturating_add(1))
            })?;
        }
        None => {}
    }
    Ok(true)
}

/// Whether an assignment's value is itself an assignment, possibly
/// parenthesized (`A = B = 1`, `A = (B = 1)`, comments aside).
fn is_chained_assignment(value: Node<'_>) -> bool {
    let mut current = value;
    for _ in 0..MAX_PATH_DEPTH {
        if current.kind() != "parenthesized_statements" {
            break;
        }
        let mut inner = named_children(current).filter(|child| child.kind() != "comment");
        match (inner.next(), inner.next()) {
            (Some(statement), None) => current = statement,
            _ => return false,
        }
    }
    current.kind() == "assignment"
}

/// `value => pattern` and `value in pattern` evaluate their subject before the
/// pattern binds anything, so a capture never turns the subject's own bare
/// call (`value => value`) into a local read.
fn visit_pattern_match(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let (Some(value), Some(pattern)) = (
        node.child_by_field_name("value"),
        node.child_by_field_name("pattern"),
    ) else {
        return Ok(false);
    };
    let next = depth.saturating_add(1);
    builder.visit(value, next)?;
    bind_targets(builder, pattern, 0)?;
    builder.visit(pattern, next)?;
    Ok(true)
}

/// `x += 1`, `rescue => e`, `for x in xs`, and `case ... in` patterns
/// (`in [a, b]`, `in {name:}`) bind locals in the current scope.
fn bind_introduced_locals(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let target = match node.kind() {
        "operator_assignment" => node.child_by_field_name("left"),
        "for" | "in_clause" => node.child_by_field_name("pattern"),
        _ => Some(node),
    };
    match target {
        Some(target) => bind_targets(builder, target, 0),
        None => Ok(()),
    }
}

/// What a receiverless call declares, if anything.
#[derive(Clone, Copy)]
enum DeclaringCall {
    Require { relative: bool },
    Visibility(Visibility),
    PrivateClassMethod,
    Accessor,
}

impl DeclaringCall {
    fn classify(method: &str) -> Option<Self> {
        match method {
            "require" => Some(Self::Require { relative: false }),
            "require_relative" => Some(Self::Require { relative: true }),
            "private_class_method" => Some(Self::PrivateClassMethod),
            _ if ACCESSOR_MACROS.contains(&method) => Some(Self::Accessor),
            _ => visibility_keyword(method).map(Self::Visibility),
        }
    }
}

/// Receiverless calls that declare rather than invoke: loads, accessor macros,
/// and visibility modifiers.
fn visit_declaring_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if node.child_by_field_name("receiver").is_some() {
        return Ok(false);
    }
    let Some(method) = node.child_by_field_name("method") else {
        return Ok(false);
    };
    match DeclaringCall::classify(builder.context.text(method).trim()) {
        Some(DeclaringCall::Require { relative }) => visit_require(builder, node, relative),
        Some(DeclaringCall::Visibility(visibility)) => visit_visibility_call(
            builder,
            MethodVisit {
                node,
                depth,
                explicit: Some(visibility),
            },
        ),
        Some(DeclaringCall::PrivateClassMethod) => restrict_named_methods(
            builder,
            NamedRestriction {
                call: node,
                visibility: Visibility::Private,
                singleton: true,
            },
        ),
        Some(DeclaringCall::Accessor) => visit_accessor(builder, node, depth),
        None => Ok(false),
    }
}

/// `require_relative 'x'` and `require './x'` load a file relative to the
/// requiring file; any other `require` names a load-path feature or gem that
/// is retained as an import but never guessed as a project file.
fn visit_require(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    relative: bool,
) -> Result<bool, ExtractError> {
    let Some(content) = sole_plain_string_argument(node) else {
        return Ok(false);
    };
    let Some(display) = bounded_name(builder, content)? else {
        return Ok(false);
    };
    let file_relative = relative || is_relative_specifier(&display);
    let specifier = if file_relative && !is_relative_specifier(&display) {
        if display.starts_with('/') {
            return Ok(false);
        }
        let prefixed = format!("./{display}");
        builder.context.copy_text(&prefixed)?
    } else {
        builder.context.copy_text(&display)?
    };
    let kind = if file_relative {
        ImportBindingKind::Namespace
    } else {
        ImportBindingKind::IncludeSystem
    };
    emit_load_import(
        builder,
        LoadImport {
            site: node,
            display,
            specifier,
            namespace: None,
            kind,
        },
    )?;
    Ok(true)
}

/// The `string_content` of a call's only argument when it is a plain,
/// interpolation-free string literal.
fn sole_plain_string_argument(call: Node<'_>) -> Option<Node<'_>> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut values = named_children(arguments);
    let string = values.next()?;
    if values.next().is_some() || string.kind() != "string" {
        return None;
    }
    let mut parts = named_children(string);
    let content = parts.next()?;
    (parts.next().is_none() && content.kind() == "string_content").then_some(content)
}

/// `private`/`protected`/`public` with no arguments opens a section, wrapping a
/// `def` applies to that method, and symbol arguments restrict methods that
/// were already defined.
fn visit_visibility_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    call: MethodVisit<'_>,
) -> Result<bool, ExtractError> {
    let MethodVisit {
        node,
        depth,
        explicit,
    } = call;
    let Some(visibility) = explicit else {
        return Ok(false);
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Ok(open_section(builder, node, visibility));
    };
    let mut values = named_children(arguments);
    match (values.next(), values.next()) {
        (Some(method), None) if method.kind() == "method" => visit_method(
            builder,
            MethodVisit {
                node: method,
                depth: depth.saturating_add(1),
                explicit: Some(visibility),
            },
        ),
        _ => restrict_named_methods(
            builder,
            NamedRestriction {
                call: node,
                visibility,
                singleton: builder.script.ruby.singleton_body,
            },
        ),
    }
}

#[derive(Clone, Copy)]
struct NamedRestriction<'tree> {
    call: Node<'tree>,
    visibility: Visibility,
    singleton: bool,
}

/// `private :a, :b` changes methods the current class already defined; it
/// stays an ordinary call when any argument is not a plain symbol.
fn restrict_named_methods(
    builder: &mut ExtractionBuilder<'_, '_>,
    restriction: NamedRestriction<'_>,
) -> Result<bool, ExtractError> {
    let Some(arguments) = restriction.call.child_by_field_name("arguments") else {
        return Ok(false);
    };
    if !inside_type(builder)
        || named_children(arguments).any(|argument| argument.kind() != "simple_symbol")
    {
        return Ok(false);
    }
    for argument in named_children(arguments) {
        builder.context.ensure_active()?;
        let name = builder
            .context
            .text(argument)
            .trim_start_matches(':')
            .trim();
        let qualified_name = builder.qualified_name(name)?;
        builder.script.ruby.restrict(MethodRestriction {
            qualified_name,
            static_member: restriction.singleton,
            visibility: restriction.visibility,
        });
    }
    Ok(true)
}

/// Apply each method's latest retroactive restriction (`private :name`) to
/// every definition emitted before it; a later redefinition keeps its own
/// visibility, and an earlier class body reopened later is restricted too.
fn apply_restrictions(builder: &mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError> {
    let restricted = builder.script.ruby.take_restrictions();
    for (position, (index, visibility)) in restricted.into_iter().enumerate() {
        if position.is_multiple_of(RESTRICTION_CANCELLATION_INTERVAL) {
            builder.context.ensure_active()?;
        }
        if let Some(symbol) = builder.facts.symbols.get_mut(index) {
            symbol.visibility = Some(visibility);
        }
    }
    Ok(())
}

/// Index the method `emit_symbol` just retained for retroactive restriction.
fn record_emitted_method(builder: &mut ExtractionBuilder<'_, '_>, static_member: bool) {
    let Some(index) = builder.facts.symbols.len().checked_sub(1) else {
        return;
    };
    let Some(qualified_name) = builder
        .facts
        .symbols
        .get(index)
        .map(|symbol| symbol.qualified_name.clone())
    else {
        return;
    };
    builder
        .script
        .ruby
        .record_method(qualified_name, static_member, index);
}

fn visit_accessor(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if !inside_type(builder)
        || node
            .parent()
            .is_none_or(|parent| parent.kind() != "body_statement")
    {
        return Ok(false);
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Ok(false);
    };
    let symbols = named_children(arguments)
        .filter(|argument| matches!(argument.kind(), "simple_symbol" | "symbol"))
        .collect::<Vec<_>>();
    if symbols.is_empty() {
        return Ok(false);
    }
    let visibility = Some(current_section(builder));
    let sole = symbols.len() == 1;
    for symbol in symbols {
        let text = builder.context.text(symbol).trim_start_matches(':');
        let Some(name) = bounded_text(builder, text)? else {
            continue;
        };
        // A sole field's structure is the whole macro call; in a list each
        // field is analysed through its own symbol, so a wide list is not
        // re-analysed once per field.
        let mut pending = plain_symbol(SymbolKind::Field, name, symbol);
        if sole {
            pending.structural_node = node;
        }
        pending.doc_anchor = node;
        pending.visibility = visibility;
        builder.emit_symbol(pending)?;
    }
    // Option values such as `default: build_defaults()` still execute.
    for option in named_children(arguments)
        .filter(|argument| !matches!(argument.kind(), "simple_symbol" | "symbol"))
    {
        builder.visit(option, depth.saturating_add(1))?;
    }
    if let Some(block) = node.child_by_field_name("block") {
        builder.visit(block, depth.saturating_add(1))?;
    }
    Ok(true)
}

/// A bare `private`/`protected`/`public` statement directly inside a class or
/// module body opens a visibility section.
fn apply_section_modifier(builder: &mut ExtractionBuilder<'_, '_>, node: Node<'_>) -> bool {
    let Some(visibility) = visibility_keyword(builder.context.text(node)) else {
        return false;
    };
    open_section(builder, node, visibility)
}

fn open_section(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    visibility: Visibility,
) -> bool {
    let in_body = node
        .parent()
        .is_some_and(|parent| SECTION_BODIES.contains(&parent.kind()));
    if !in_body || !inside_type(builder) {
        return false;
    }
    set_section(builder, Some(visibility));
    true
}

fn visibility_keyword(text: &str) -> Option<Visibility> {
    match text.trim() {
        "private" => Some(Visibility::Private),
        "protected" => Some(Visibility::Protected),
        "public" => Some(Visibility::Public),
        _ => None,
    }
}

fn set_section(builder: &mut ExtractionBuilder<'_, '_>, visibility: Option<Visibility>) {
    if let Some(slot) = builder.native_visibilities.last_mut() {
        *slot = visibility;
    }
}

fn current_section(builder: &ExtractionBuilder<'_, '_>) -> Visibility {
    builder
        .native_visibilities
        .last()
        .copied()
        .flatten()
        .unwrap_or(Visibility::Public)
}

fn inside_type(builder: &ExtractionBuilder<'_, '_>) -> bool {
    builder
        .native_owner_kinds
        .last()
        .is_some_and(|kind| matches!(kind, SymbolKind::Class | SymbolKind::Module))
}

/// `A`, `A::B`, or `::A`: a constant path with no computed scope.
fn is_constant_path(node: Node<'_>, depth: usize) -> bool {
    if depth > MAX_PATH_DEPTH {
        return false;
    }
    match node.kind() {
        "constant" => true,
        "scope_resolution" => {
            node.child_by_field_name("name")
                .is_some_and(|name| name.kind() == "constant")
                && node
                    .child_by_field_name("scope")
                    .is_none_or(|scope| is_constant_path(scope, depth.saturating_add(1)))
        }
        _ => false,
    }
}

fn capture_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let Some(method) = node.child_by_field_name("method") else {
        return Ok(());
    };
    let Some(name) = call_name(builder, node, 0)? else {
        return Ok(());
    };
    let owner = builder.owners.last().cloned();
    references::push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: ReferenceKind::Calls,
            node: method,
        },
    )
}

/// v1 call naming: `Receiver.method` for named receivers, the bare method for
/// `self`/`super` or literal receivers, and `X.new.m` folded to `X.m`.
/// Arguments never contribute to a name.
fn call_name(
    builder: &ExtractionBuilder<'_, '_>,
    call: Node<'_>,
    depth: usize,
) -> Result<Option<String>, ExtractError> {
    if depth > MAX_RECEIVER_DEPTH {
        return Ok(None);
    }
    let Some(method) = call.child_by_field_name("method") else {
        return Ok(None);
    };
    let Some(method_name) = bounded_name(builder, method)? else {
        return Ok(None);
    };
    let Some(receiver) = call.child_by_field_name("receiver") else {
        return Ok(Some(method_name));
    };
    let receiver_name = if SELF_RECEIVERS.contains(&receiver.kind()) {
        None
    } else if NAMED_RECEIVERS.contains(&receiver.kind()) || is_constant_path(receiver, 0) {
        bounded_name(builder, receiver)?
    } else if receiver.kind() == "call" {
        folded_receiver(
            builder,
            ReceiverFold {
                receiver,
                method_name: &method_name,
                depth,
            },
        )?
    } else {
        None
    };
    let Some(receiver_name) = receiver_name else {
        return Ok(Some(method_name));
    };
    let qualified = format!("{receiver_name}.{method_name}");
    Ok(bounded_reference_name(builder, &qualified)?.or(Some(method_name)))
}

/// A receiver that is itself a call, named for the method invoked on its result.
#[derive(Clone, Copy)]
struct ReceiverFold<'tree, 'name> {
    receiver: Node<'tree>,
    method_name: &'name str,
    depth: usize,
}

fn folded_receiver(
    builder: &ExtractionBuilder<'_, '_>,
    fold: ReceiverFold<'_, '_>,
) -> Result<Option<String>, ExtractError> {
    let Some(nested) = call_name(builder, fold.receiver, fold.depth.saturating_add(1))? else {
        return Ok(None);
    };
    match nested.strip_suffix(".new") {
        Some(constructed) if fold.method_name != "new" => bounded_text(builder, constructed),
        _ => Ok(Some(nested)),
    }
}

/// A statement-level identifier is a receiverless, argument-less call unless
/// an earlier binding in scope makes it a local variable read.
fn capture_bare_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node
        .parent()
        .is_none_or(|parent| !BARE_CALL_PARENTS.contains(&parent.kind()))
    {
        return Ok(());
    }
    let text = builder.context.text(node).trim();
    if text.is_empty()
        || BARE_CALL_SKIP_NAMES.contains(&text)
        || starts_uppercase(text)
        || builder.script.ruby.locals.is_local(text)
    {
        return Ok(());
    }
    let Some(name) = bounded_text(builder, text)? else {
        return Ok(());
    };
    let owner = builder.owners.last().cloned();
    references::push_reference(
        builder,
        PendingReference {
            owner,
            name,
            kind: ReferenceKind::Calls,
            node,
        },
    )
}
