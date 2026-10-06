//! `ArkTS` (`HarmonyOS`) structural extraction.
//!
//! `ArkTS` is TypeScript with `ArkUI` additions, and its grammar keeps
//! TypeScript's node kinds. Classes, interfaces, functions, methods, bindings,
//! imports, exports, type aliases, and enums therefore go through the
//! JavaScript-family walker unchanged. This family adds what `ArkTS` needs on
//! top: `struct` components, class and struct fields, and decorators
//! (`@Component`, `@State`, `@Builder`, ...) recorded as `Decorates`
//! references owned by the declaration they decorate. Members and enum
//! constants named by string or computed keys are expressions, not
//! identifiers, so they never become symbol names.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{ExtractError, SymbolExportFlags};

use super::{
    ExtractionBuilder, PendingReference, PendingSymbol, SymbolScope, in_symbol_scope, references,
    syntax::{export_flags, has_child_kind, named_children, span_for, visibility},
    visit_javascript_declaration,
};

/// Decorators inspected before one member; `ArkUI` stacks at most a few.
const MAXIMUM_STACKED_DECORATORS: usize = 32;
/// Member-access hops followed to reach a decorator's final name segment.
const MAXIMUM_DECORATOR_NAME_DEPTH: usize = 8;

/// Extract one `ArkTS` declaration, returning whether `node` was consumed.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match node.kind() {
        "program" if node.parent().is_none() => {
            builder.visit_named_children(node, depth)?;
            drop_commented_signatures(builder)?;
        }
        "struct_declaration" => visit_struct(builder, node, depth)?,
        "public_field_definition" => visit_field(builder, node, depth)?,
        // Decorators are recorded by the declaration they decorate.
        "decorator" => {}
        "method_definition" | "method_signature" | "abstract_method_signature"
            if !has_identifier_name(node) =>
        {
            // Keep the key expression's and body's facts in the enclosing
            // scope, but never copy the key into a symbol name.
            for part in ["name", "body"]
                .into_iter()
                .filter_map(|field| node.child_by_field_name(field))
            {
                builder.visit(part, depth.saturating_add(1))?;
            }
        }
        "enum_declaration" => visit_enum(builder, node)?,
        "class_declaration"
        | "abstract_class_declaration"
        | "function_declaration"
        | "method_definition" => {
            let first_emitted = builder.facts.symbols.len();
            let first_reference = builder.facts.references.len();
            visit_javascript_declaration(builder, node, depth)?;
            if let Some(owner) = emitted_symbol_for(builder, first_emitted, node)?
                && !decorated_by_shared_walker(builder, first_reference, &owner)
            {
                capture_decorators(builder, node, &owner)?;
            }
        }
        _ => return visit_javascript_declaration(builder, node, depth),
    }
    Ok(true)
}

/// The shared TypeScript renderer copies parameter and alias text verbatim;
/// a comment inside it is free text, so such signatures are not retained.
fn drop_commented_signatures(builder: &mut ExtractionBuilder<'_, '_>) -> Result<(), ExtractError> {
    for symbol in &mut builder.facts.symbols {
        if (builder.context.cancelled)() {
            return Err(ExtractError::Cancelled);
        }
        if symbol
            .signature
            .as_deref()
            .is_some_and(|signature| signature.contains("/*") || signature.contains("//"))
        {
            symbol.signature = None;
        }
    }
    Ok(())
}

/// Whether a class member is named by a plain identifier.
fn has_identifier_name(member: Node<'_>) -> bool {
    member.child_by_field_name("name").is_some_and(|name| {
        matches!(
            name.kind(),
            "property_identifier" | "private_property_identifier" | "identifier"
        )
    })
}

/// Emit an enum and the constants it names with identifiers.
fn visit_enum(builder: &mut ExtractionBuilder<'_, '_>, node: Node<'_>) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let name = builder.context.owned_text(name_node)?;
    let (exported, default_export) = export_flags(node);
    let id = builder.emit_symbol(PendingSymbol {
        export: SymbolExportFlags::new(exported, default_export),
        ..PendingSymbol::plain(SymbolKind::Enum, name.clone(), node)
    })?;
    let body = node.child_by_field_name("body");
    let scope = SymbolScope {
        id,
        kind: SymbolKind::Enum,
        name,
    };
    in_symbol_scope(builder, scope, |builder| {
        for member in body.into_iter().flat_map(named_children) {
            builder.context.ensure_active()?;
            let constant = match member.kind() {
                "enum_assignment" => member.child_by_field_name("name"),
                _ => Some(member),
            };
            let Some(constant) =
                constant.filter(|constant| constant.kind() == "property_identifier")
            else {
                continue;
            };
            let constant_name = builder.context.owned_text(constant)?;
            builder.emit_symbol(PendingSymbol::plain(
                SymbolKind::EnumMember,
                constant_name,
                member,
            ))?;
        }
        Ok(())
    })
}

/// The symbol the JavaScript walker emitted first while visiting `node`,
/// provided it is the declaration of `node` itself (same exact span).
fn emitted_symbol_for(
    builder: &ExtractionBuilder<'_, '_>,
    first_emitted: usize,
    node: Node<'_>,
) -> Result<Option<SymbolId>, ExtractError> {
    let span = span_for(node)?;
    Ok(builder
        .facts
        .symbols
        .get(first_emitted)
        .filter(|symbol| symbol.span == span)
        .map(|symbol| symbol.id.clone()))
}

/// Emit a `struct` component with its decorators and members.
fn visit_struct(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let Some(name_node) = node.child_by_field_name("name") else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let (exported, default_export) = export_flags(node);
    let id = builder.emit_symbol(PendingSymbol {
        body_node: node.child_by_field_name("body"),
        export: SymbolExportFlags::new(exported, default_export),
        ..PendingSymbol::plain(SymbolKind::Struct, name.clone(), node)
    })?;
    capture_decorators(builder, node, &id)?;
    let body = node.child_by_field_name("body");
    let scope = SymbolScope {
        id,
        kind: SymbolKind::Struct,
        name,
    };
    in_symbol_scope(builder, scope, |builder| match body {
        Some(body) => builder.visit_named_children(body, depth.saturating_add(1)),
        None => Ok(()),
    })
}

/// Emit a class or struct field with its decorators and initializer.
fn visit_field(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let name_node = node.child_by_field_name("name").filter(|name| {
        matches!(
            name.kind(),
            "property_identifier" | "private_property_identifier"
        )
    });
    let Some(name_node) = name_node else {
        return builder.visit_named_children(node, depth);
    };
    let name = builder.context.owned_text(name_node)?;
    let value = node.child_by_field_name("value");
    let id = builder.emit_symbol(PendingSymbol {
        body_node: value,
        static_member: has_child_kind(node, "static"),
        visibility: visibility(node, builder.context.source()),
        ..PendingSymbol::plain(SymbolKind::Field, name.clone(), node)
    })?;
    capture_decorators(builder, node, &id)?;
    if let Some(type_annotation) = node.child_by_field_name("type") {
        references::capture_type_nodes(builder, type_annotation, &id)?;
    }
    let Some(value) = value else {
        return Ok(());
    };
    let scope = SymbolScope {
        id,
        kind: SymbolKind::Field,
        name,
    };
    in_symbol_scope(builder, scope, |builder| {
        builder.visit(value, depth.saturating_add(1))
    })
}

/// Whether the shared JavaScript walker already recorded decorators for
/// `owner` while visiting it (references from `first_reference` on), so this
/// family must not record them a second time.
fn decorated_by_shared_walker(
    builder: &ExtractionBuilder<'_, '_>,
    first_reference: usize,
    owner: &SymbolId,
) -> bool {
    builder.facts.references[first_reference..]
        .iter()
        .any(|reference| {
            reference.kind == ReferenceKind::Decorates && reference.owner.as_ref() == Some(owner)
        })
}

/// Record every decorator of `node` as a `Decorates` reference from `owner`:
/// its own decorator children, those of an enclosing `export` statement, and
/// (for class members) the decorators stacked directly before it in the body.
fn capture_decorators(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    owner: &SymbolId,
) -> Result<(), ExtractError> {
    let export = node
        .parent()
        .filter(|parent| parent.kind() == "export_statement");
    let own = named_children(node).filter(|child| child.kind() == "decorator");
    let exported = export
        .into_iter()
        .flat_map(named_children)
        .filter(|child| child.kind() == "decorator");
    let stacked = (node.kind() == "method_definition")
        .then(|| preceding_decorators(node))
        .into_iter()
        .flatten();
    for decorator in own.chain(exported).chain(stacked) {
        builder.context.ensure_active()?;
        let Some(name) = decorator_name(decorator) else {
            continue;
        };
        let name = builder.context.owned_text(name)?;
        references::push_reference(
            builder,
            PendingReference {
                owner: Some(owner.clone()),
                name,
                kind: ReferenceKind::Decorates,
                node: decorator,
            },
        )?;
    }
    Ok(())
}

/// The decorators stacked immediately before a class member (comments may sit
/// between them), stopping at the first other node so an earlier member's
/// decorators never leak forward.
fn preceding_decorators(member: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(member.prev_named_sibling(), Node::prev_named_sibling)
        .take(MAXIMUM_STACKED_DECORATORS)
        .take_while(|sibling| matches!(sibling.kind(), "decorator" | "comment"))
        .filter(|sibling| sibling.kind() == "decorator")
}

/// The identifier naming a decorator: `@Name`, `@Name(args)`, or the last
/// segment of `@a.b.Name(args)`.
fn decorator_name(decorator: Node<'_>) -> Option<Node<'_>> {
    let mut target = decorator.named_child(0)?;
    for _ in 0..MAXIMUM_DECORATOR_NAME_DEPTH {
        target = match target.kind() {
            "identifier" | "property_identifier" => return Some(target),
            "call_expression" => target.child_by_field_name("function")?,
            "member_expression" => target.child_by_field_name("property")?,
            _ => return None,
        };
    }
    None
}
