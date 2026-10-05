//! JavaScript/TypeScript decorator references.
//!
//! A decorator applied to a class, method, or field becomes a
//! [`ReferenceKind::Decorates`] reference owned by the decorated symbol and
//! named by the decorator function (`@Get('/x')` and `@Get` both name `Get`;
//! `@ng.Input()` names the static chain `ng.Input`, which namespace-import
//! resolution binds exactly). A decorator whose name an enclosing parameter
//! or local rebinds (`function f(ng) { @ng.Input() class C {} }`) names that
//! binding, not the import, and emits nothing when the resolver could not
//! tell them apart. Decorator arguments are not retained: framework
//! scanners read the literal arguments they need from source themselves.
//!
//! Decorators reach a declaration in three grammar shapes: as its own
//! `decorator` children (classes, JavaScript methods, fields), as children of
//! an enclosing `export_statement` (`@Dec export class X {}`), and — for
//! TypeScript methods — as `class_body` siblings immediately preceding the
//! method. The sibling scan stops at the first non-decorator so a decorator
//! is never attributed to a later member.

use cartograph_domain::{ReferenceKind, SymbolId};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    ExtractionBuilder, PendingReference, javascript_scopes, references, syntax::named_children,
};

/// Emit `decorates` references for every decorator applied to `declaration`.
pub(super) fn capture_declaration_decorators(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for decorator in preceding_member_decorators(builder, declaration)? {
        capture_decorator(builder, decorator, owner)?;
    }
    if let Some(export) = declaration
        .parent()
        .filter(|parent| parent.kind() == "export_statement")
    {
        capture_child_decorators(builder, export, owner)?;
    }
    capture_child_decorators(builder, declaration, owner)
}

/// Decorators that are direct `decorator` children of `node`.
fn capture_child_decorators(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    for decorator in named_children(node).filter(|child| child.kind() == "decorator") {
        capture_decorator(builder, decorator, owner)?;
    }
    Ok(())
}

/// TypeScript method decorators, in source order. Comments between stacked
/// decorators are trivia; any other member ends the scan, so each scan stops
/// at the previous member and the scans of one class body stay linear.
fn preceding_member_decorators<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'tree>,
) -> Result<Vec<Node<'tree>>, ExtractError> {
    let mut decorators = Vec::new();
    if declaration.kind() != "method_definition"
        || declaration
            .parent()
            .is_none_or(|parent| parent.kind() != "class_body")
    {
        return Ok(decorators);
    }
    let mut sibling = declaration.prev_named_sibling();
    while let Some(candidate) = sibling {
        builder.context.ensure_active()?;
        sibling = candidate.prev_named_sibling();
        match candidate.kind() {
            "comment" => {}
            "decorator" => {
                decorators
                    .try_reserve(1)
                    .map_err(|_| ExtractError::OutputLimit)?;
                decorators.push(candidate);
            }
            _ => break,
        }
    }
    decorators.reverse();
    Ok(decorators)
}

/// One `decorates` reference named by the decorator's static target.
fn capture_decorator(
    builder: &mut ExtractionBuilder<'_, '_>,
    decorator: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    builder.context.ensure_active()?;
    let Some(target) = decorator_target(decorator) else {
        return Ok(());
    };
    if !javascript_scopes::static_chain_resolves(builder, target)? {
        return Ok(());
    }
    let Some(name) = references::static_member_chain_name(builder, target)? else {
        return Ok(());
    };
    references::push_reference(
        builder,
        PendingReference {
            owner: Some(owner.clone()),
            name,
            kind: ReferenceKind::Decorates,
            node: target,
        },
    )
}

/// The decorator function: the callee of `@Dec(..)` or the bare `@Dec`.
/// Computed receivers and nested calls have no static name and yield nothing.
fn decorator_target(decorator: Node<'_>) -> Option<Node<'_>> {
    let expression = named_children(decorator).find(|child| child.kind() != "comment")?;
    if expression.kind() == "call_expression" {
        expression.child_by_field_name("function")
    } else {
        Some(expression)
    }
}
