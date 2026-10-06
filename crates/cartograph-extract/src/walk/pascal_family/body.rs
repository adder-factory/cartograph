//! Call references inside Pascal routine bodies, program blocks, and unit
//! initialization/finalization sections.
//!
//! Pascal calls a parameterless routine without parentheses, so a statement
//! that is only a designator (`DoWork;`, `Logger.Flush;`) is a call as well as
//! every `exprCall`. Names bind innermost-first through the enclosing
//! routines' scopes (a parameter or local is a procedural value; a nested
//! routine, or the routine itself, is a routine), then through members of the
//! enclosing class and its in-file ancestors (implicit `Self`), then through
//! ordinary resolution. A binding the file proves is sent as a self-scope
//! resolution name, which the indexer looks up by exact same-file qualified
//! name and never widens to a project-wide guess. A call whose target the file
//! cannot pin down (a procedural value, an overloaded routine or member,
//! anything inside a `with` body, a member of a computed or shadowed receiver)
//! carries the same marker without a qualifier, so it stays explicitly
//! unresolved instead of binding to an unrelated global of that name.

use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SymbolId};
use tree_sitter::Node;

use super::{
    MAX_AST_DEPTH,
    index::{FileIndex, MemberBinding, MemberQuery, MemberUse},
    names::{identifier_text, name_parts},
    routines::LexicalScope,
};
use crate::{
    ExtractError, ExtractedReference, RUST_SELF_RECEIVER_RESOLUTION_PREFIX,
    walk::{AstVisitBudget, ExtractionBuilder, syntax::named_children, syntax::span_for},
};

/// Longest call designator retained as a reference name.
const MAXIMUM_DESIGNATOR_BYTES: usize = 512;
/// Dotted components needed for `Unit.Type.Member` (unit, type, member).
const MINIMUM_UNIT_MEMBER_PARTS: usize = 3;

/// Module prefix that marks a top-level same-file target for exact lookup.
const SAME_FILE_MODULE_PREFIX: &str = "self::";

/// Control-flow intrinsics that read like parameterless calls.
const CONTROL_FLOW_INTRINSICS: &[&str] = &["break", "continue", "exit"];

/// Lexical facts about the routine (or program block) that owns a body.
pub(super) struct CallScope<'scope, 'tree> {
    pub(super) owner: &'scope SymbolId,
    pub(super) class_key: Option<&'scope str>,
    pub(super) scopes: &'scope [LexicalScope],
    pub(super) index: &'scope FileIndex<'tree>,
}

/// One body to scan for calls.
pub(super) struct BodyCapture<'scope, 'tree> {
    pub(super) root: Node<'tree>,
    pub(super) depth: usize,
    pub(super) scope: CallScope<'scope, 'tree>,
}

/// How a call's name is bound before project resolution.
enum CallBinding {
    /// Ordinary name resolution.
    Open,
    /// The file proves the declared target's same-file qualified name.
    Scoped(String),
    /// An explicit unit path whose member separators are normalized, still
    /// resolved through the unit's import binding.
    UnitPath(String),
    /// The target depends on something the file cannot resolve.
    Uncertain,
}

/// What an enclosing scope declares under a simple name.
enum LocalName<'scope> {
    Value,
    Routine(&'scope str),
}

/// A node still to scan, and whether it sits inside a `with` body.
#[derive(Clone, Copy)]
struct PendingNode<'tree> {
    node: Node<'tree>,
    depth: usize,
    in_with: bool,
}

/// Emit a `Calls` reference for every call site under `capture.root`.
pub(super) fn capture_calls(
    builder: &mut ExtractionBuilder<'_, '_>,
    budget: &mut AstVisitBudget<MAX_AST_DEPTH>,
    capture: &BodyCapture<'_, '_>,
) -> Result<(), ExtractError> {
    let mut stack = vec![PendingNode {
        node: capture.root,
        depth: capture.depth,
        in_with: false,
    }];
    let mut inline = BTreeSet::new();
    while let Some(pending) = stack.pop() {
        budget.observe(builder, pending.depth)?;
        let target = match pending.node.kind() {
            "asm" | "comment" | "pp" => continue,
            "exprCall" => call_target(pending.node),
            "statement" => statement_designator(pending.node),
            "varDef" | "varAssignDef" => {
                declare_inline(builder, pending.node, &mut inline)?;
                None
            }
            _ => None,
        };
        if let Some(target) = target {
            let binder = Binder {
                scope: &capture.scope,
                inline: &inline,
            };
            emit_call(builder, &binder, CallSite::new(target, pending))?;
        }
        push_children(&mut stack, pending);
    }
    Ok(())
}

/// A body's inline `var X := ...` declares a value visible to the
/// statements after it (conservatively, to the end of the body).
fn declare_inline(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    inline: &mut BTreeSet<String>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let Some(name) = named_children(declaration)
        .find(|child| child.kind() == "identifier")
        .and_then(|name| identifier_text(source, name))
    else {
        return Ok(());
    };
    let lower = name.to_ascii_lowercase();
    builder.context.budget.reserve_additional_string(&lower)?;
    inline.insert(lower);
    Ok(())
}

/// The scope a call is bound in: the enclosing routines' scopes plus the
/// inline values the body has declared so far.
struct Binder<'binder, 'scope, 'tree> {
    scope: &'binder CallScope<'scope, 'tree>,
    inline: &'binder BTreeSet<String>,
}

/// Queue a node's children in source order, marking a `with` body.
fn push_children<'tree>(stack: &mut Vec<PendingNode<'tree>>, parent: PendingNode<'tree>) {
    let with_body = (parent.node.kind() == "with")
        .then(|| parent.node.child_by_field_name("body"))
        .flatten();
    let depth = parent.depth.saturating_add(1);
    let children = named_children(parent.node).collect::<Vec<_>>();
    for child in children.into_iter().rev() {
        stack.push(PendingNode {
            node: child,
            depth,
            in_with: parent.in_with || with_body == Some(child),
        });
    }
}

/// The callee of an `exprCall` when it is a plain or dotted designator.
fn call_target(call: Node<'_>) -> Option<Node<'_>> {
    let entity = call
        .child_by_field_name("entity")
        .or_else(|| named_children(call).next())?;
    matches!(entity.kind(), "identifier" | "exprDot").then_some(entity)
}

/// A statement consisting only of a designator is a parameterless call.
fn statement_designator(statement: Node<'_>) -> Option<Node<'_>> {
    let mut children =
        named_children(statement).filter(|child| !matches!(child.kind(), "comment" | "pp"));
    let only = children.next()?;
    (children.next().is_none() && matches!(only.kind(), "identifier" | "exprDot")).then_some(only)
}

/// One call site: its callee designator and the syntax around it.
#[derive(Clone, Copy)]
struct CallSite<'tree> {
    target: Node<'tree>,
    bare_statement: bool,
    in_with: bool,
}

impl<'tree> CallSite<'tree> {
    /// The call whose callee is `target`, found at `pending`.
    fn new(target: Node<'tree>, pending: PendingNode<'tree>) -> Self {
        Self {
            target,
            bare_statement: pending.node.kind() == "statement",
            in_with: pending.in_with,
        }
    }
}

/// Emit one `Calls` reference with its scope binding.
fn emit_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    binder: &Binder<'_, '_, '_>,
    site: CallSite<'_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let (parts, binding) = if let Some(parts) = name_parts(source, site.target) {
        let binding = if site.in_with {
            CallBinding::Uncertain
        } else {
            call_binding(binder, &parts)
        };
        (parts, binding)
    } else {
        let Some(rightmost) = rightmost_identifier(source, site.target) else {
            return Ok(());
        };
        (vec![rightmost], CallBinding::Uncertain)
    };
    let Some(name) = bounded_designator(&parts) else {
        return Ok(());
    };
    if site.bare_statement && is_control_flow(&parts) {
        return Ok(());
    }
    let resolution_name = match binding {
        CallBinding::Open => None,
        CallBinding::Scoped(qualified) => Some(self_scope(&same_file_target(&qualified))),
        CallBinding::UnitPath(path) => Some(path),
        CallBinding::Uncertain => Some(self_scope(&name)),
    };
    builder.emit_reference(ExtractedReference {
        owner: Some(binder.scope.owner.clone()),
        name: builder.context.copy_text(&name)?,
        resolution_name,
        kind: ReferenceKind::Calls,
        span: span_for(site.target)?,
    })
}

/// The self-scope lookup name for a same-file qualified target. Without a
/// `::` qualifier the indexer records the reference as an unresolved
/// receiver dispatch instead of guessing.
pub(super) fn self_scope(target: &str) -> String {
    let mut name = String::with_capacity(
        RUST_SELF_RECEIVER_RESOLUTION_PREFIX
            .len()
            .saturating_add(target.len()),
    );
    name.push_str(RUST_SELF_RECEIVER_RESOLUTION_PREFIX);
    name.push_str(target);
    name
}

/// A same-file target in the form the self-scope lookup binds exactly. A
/// top-level routine's qualified name has no `::`, so it takes the `self::`
/// module prefix, which the lookup strips once before its exact same-file
/// match; a name that itself starts with `self::` (a routine spelled
/// `&self`) takes the prefix too, so its own qualifier is never consumed.
pub(super) fn same_file_target(qualified: &str) -> String {
    if qualified.contains("::") && !qualified.starts_with(SAME_FILE_MODULE_PREFIX) {
        qualified.to_owned()
    } else {
        format!("{SAME_FILE_MODULE_PREFIX}{qualified}")
    }
}

/// `Items[I].Run` keeps the member name; its receiver is unknown.
fn rightmost_identifier<'source>(source: &'source str, target: Node<'_>) -> Option<&'source str> {
    let rightmost = target.child_by_field_name("rhs")?;
    (rightmost.kind() == "identifier")
        .then(|| identifier_text(source, rightmost))
        .flatten()
}

/// The dotted designator, when it fits the reference-name bound.
fn bounded_designator(parts: &[&str]) -> Option<String> {
    let name = parts.join(".");
    (name.len() <= MAXIMUM_DESIGNATOR_BYTES).then_some(name)
}

/// `Exit`, `Break`, and `Continue` used as bare statements.
fn is_control_flow(parts: &[&str]) -> bool {
    matches!(parts, [only] if CONTROL_FLOW_INTRINSICS
        .iter()
        .any(|intrinsic| intrinsic.eq_ignore_ascii_case(only)))
}

/// Bind a designator by Pascal scope before project resolution. A member of
/// a `Self` member (`Self.Settings.Load`) has a receiver of unknown type.
fn call_binding(binder: &Binder<'_, '_, '_>, parts: &[&str]) -> CallBinding {
    match parts {
        [name] => unqualified_binding(binder, name),
        [receiver, member] if receiver.eq_ignore_ascii_case("self") => binder
            .scope
            .class_key
            .map_or(CallBinding::Open, |class_key| {
                member_call(binder.scope, class_key, member)
            }),
        [receiver, _, _, ..] if receiver.eq_ignore_ascii_case("self") => CallBinding::Uncertain,
        [_, _, ..] => qualified_binding(binder, parts),
        [] => CallBinding::Open,
    }
}

/// Innermost first: a value or routine the body or an enclosing routine
/// declares under that name, then an implicit `Self` member, then a
/// unit-level routine of this file (Pascal names are case-insensitive, so it
/// binds by its declared spelling), which stays uncertain when overloaded.
fn unqualified_binding(binder: &Binder<'_, '_, '_>, name: &str) -> CallBinding {
    let lower = name.to_ascii_lowercase();
    match local_name(binder, &lower) {
        Some(LocalName::Value) => return CallBinding::Uncertain,
        Some(LocalName::Routine(qualified)) => return CallBinding::Scoped(qualified.to_owned()),
        None => {}
    }
    let scope = binder.scope;
    if let Some(type_key) = scope.class_key {
        match scope.index.member_binding(MemberQuery {
            type_key,
            member: name,
            usage: MemberUse::Call,
        }) {
            MemberBinding::Exact(qualified) => return CallBinding::Scoped(qualified),
            MemberBinding::Uncertain => return CallBinding::Uncertain,
            // An inherited member could shadow a same-named unit routine of this
            // file, so that routine is not an exact target; with no local
            // homonym, ordinary (import and project) resolution still applies.
            MemberBinding::UnknownAncestry if scope.index.unit_routine(&lower).is_some() => {
                return CallBinding::Uncertain;
            }
            MemberBinding::UnknownAncestry | MemberBinding::Absent => {}
        }
    }
    if scope.index.is_overloaded_routine(&lower) {
        return CallBinding::Uncertain;
    }
    scope
        .index
        .unit_routine(&lower)
        .map_or(CallBinding::Open, |spelling| {
            CallBinding::Scoped(spelling.to_owned())
        })
}

/// `Receiver.Member`: a receiver the body or an enclosing routine declares,
/// or a member of the enclosing class (Delphi member scope hides types and
/// units), is a value whose type is unknown; an in-file type names its member
/// exactly; a used unit keeps import resolution with member separators
/// normalized.
fn qualified_binding(binder: &Binder<'_, '_, '_>, parts: &[&str]) -> CallBinding {
    let Some((member, receiver)) = parts.split_last() else {
        return CallBinding::Open;
    };
    let Some(first) = receiver.first() else {
        return CallBinding::Open;
    };
    if local_name(binder, &first.to_ascii_lowercase()).is_some()
        || is_enclosing_member(binder.scope, first)
    {
        return CallBinding::Uncertain;
    }
    let scope = binder.scope;
    if let Some(type_key) = in_file_type_key(scope, &receiver.join(".").to_ascii_lowercase()) {
        return member_call(scope, &type_key, member);
    }
    unit_member_path(scope.index, parts).map_or(CallBinding::Open, CallBinding::UnitPath)
}

/// Whether `name` is any member (field, property, or routine) of the
/// enclosing class or its in-file ancestry.
fn is_enclosing_member(scope: &CallScope<'_, '_>, name: &str) -> bool {
    scope.class_key.is_some_and(|type_key| {
        !matches!(
            scope.index.member_binding(MemberQuery {
                type_key,
                member: name,
                usage: MemberUse::Accessor,
            }),
            MemberBinding::Absent | MemberBinding::UnknownAncestry
        )
    })
}

/// The innermost declaration of `lower`: an inline value of this body, then
/// the enclosing routines' scopes from the innermost out. A local routine
/// announced `forward` whose body is not emitted yet has no known target.
fn local_name<'binder>(
    binder: &Binder<'binder, '_, '_>,
    lower: &str,
) -> Option<LocalName<'binder>> {
    if binder.inline.contains(lower) {
        return Some(LocalName::Value);
    }
    binder.scope.scopes.iter().rev().find_map(|level| {
        if level.values.contains(lower) || level.forward.contains(lower) {
            Some(LocalName::Value)
        } else {
            level
                .routines
                .get(lower)
                .map(|qualified| LocalName::Routine(qualified.as_str()))
        }
    })
}

/// A member call through a receiver the file proves (`Self`, an in-file
/// type): a member missing from the in-file ancestry may still be inherited
/// from a type declared elsewhere, so it stays uncertain rather than global.
fn member_call(scope: &CallScope<'_, '_>, type_key: &str, member: &str) -> CallBinding {
    match scope.index.member_binding(MemberQuery {
        type_key,
        member,
        usage: MemberUse::Call,
    }) {
        MemberBinding::Exact(qualified) => CallBinding::Scoped(qualified),
        MemberBinding::Uncertain | MemberBinding::Absent | MemberBinding::UnknownAncestry => {
            CallBinding::Uncertain
        }
    }
}

/// `Unit.Type.Member` for a unit this file uses: the unit stays a dotted
/// prefix for its import binding and the member path takes the declared
/// `Type::Member` form (`Unit.Member` already resolves unchanged).
fn unit_member_path(index: &FileIndex<'_>, parts: &[&str]) -> Option<String> {
    if parts.len() < MINIMUM_UNIT_MEMBER_PARTS {
        return None;
    }
    let last_unit_end = parts.len().saturating_sub(2);
    (1..=last_unit_end).rev().find_map(|split| {
        let (unit, path) = parts.split_at(split);
        let unit = unit.join(".");
        index
            .uses_unit(&unit)
            .then(|| format!("{unit}.{}", path.join("::")))
    })
}

/// A receiver names an in-file type directly, or a type nested in the
/// enclosing class (visible unqualified inside its methods).
fn in_file_type_key(scope: &CallScope<'_, '_>, receiver: &str) -> Option<String> {
    if let Some(class_key) = scope.class_key {
        let nested = format!("{class_key}.{receiver}");
        if scope.index.has_type(&nested) {
            return Some(nested);
        }
    }
    scope.index.has_type(receiver).then(|| receiver.to_owned())
}
