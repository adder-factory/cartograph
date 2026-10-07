//! A constructor spelling is insufficient when an explicit callable can replace it.

use super::super::{ExtractionBuilder, record_non_method};
use super::{
    BindingType, ExtractError, ExtractionContext, NamedBind, Node, ReceiverTypes, ScopeKind,
    SourceLanguage, SyntaxIndex, TypeQuery, Visit, named_children, node_text, prefixed_type,
};

const CONSTRUCTOR_PREFIX: &str = "constructor::";
const UNSUPPORTED_CONSTRUCTOR: &str = "constructor::?";

pub(in super::super) fn explicit_type(
    types: &ReceiverTypes<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (TypeQuery<'_>, bool),
) -> Result<Option<String>, ExtractError> {
    let (query, constructed) = input;
    let marker = types.explicit_type(context, query)?;
    if !constructed {
        return Ok(marker);
    }
    marker
        .map(|marker| prefixed_type(context, (CONSTRUCTOR_PREFIX, &marker)))
        .transpose()
}

pub(super) fn class_id(marker: &str) -> Option<&str> {
    marker
        .strip_prefix(CONSTRUCTOR_PREFIX)
        .unwrap_or(marker)
        .strip_prefix('@')
}

pub(in super::super) fn bind_imports(
    builder: &mut ExtractionBuilder<'_, '_>,
    index: &mut SyntaxIndex<'_>,
    scope: usize,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Kotlin {
        return Ok(());
    }
    for binding in &builder.facts.import_bindings {
        builder.context.ensure_active()?;
        if binding.local_name == "*" {
            index.fence(&mut builder.context, scope)?;
            continue;
        }
        index.bind_name(
            &mut builder.context,
            NamedBind {
                scope,
                name: &binding.local_name,
                kind: BindingType::Import,
                start: 0,
            },
        )?;
    }
    Ok(())
}

pub(super) fn collect(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    if context.snapshot.language() == SourceLanguage::Kotlin {
        return kotlin(index, context, visit);
    }
    if context.snapshot.language() != SourceLanguage::Ruby {
        return Ok(());
    }
    if visit.node.kind() == "class" && visit.node.child_by_field_name("superclass").is_some() {
        return class_fence(index, context, (visit, Some(UNSUPPORTED_CONSTRUCTOR)));
    }
    let Some(name) = visit.node.child_by_field_name("name") else {
        return Ok(());
    };
    if node_text(context, name) != "new" {
        return Ok(());
    }
    if visit.node.kind() == "singleton_method" {
        return singleton_factory(index, context, visit);
    }
    if visit.node.kind() == "method" && inside_singleton(context, visit.node)? {
        return unknown_factory(index, context);
    }
    Ok(())
}

fn kotlin(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    match visit.node.kind() {
        "import_header" => unqualified_import(index, context, visit),
        "class_declaration" | "object_declaration" => {
            if default_constructor(context, visit.node)? {
                return Ok(());
            }
            class_fence(index, context, (visit, Some(UNSUPPORTED_CONSTRUCTOR)))
        }
        _ => Ok(()),
    }
}

fn default_constructor(
    context: &mut ExtractionContext<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let mut class = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        context.ensure_active()?;
        match child.kind() {
            "class" => class = true,
            "modifiers" | "primary_constructor" | "type_parameters" | "type_constraints" => {
                return Ok(false);
            }
            "class_body" if has_secondary_constructor(context, child)? => return Ok(false),
            _ => {}
        }
    }
    Ok(class)
}

fn has_secondary_constructor(
    context: &mut ExtractionContext<'_, '_>,
    body: Node<'_>,
) -> Result<bool, ExtractError> {
    for child in named_children(body) {
        context.ensure_active()?;
        if child.kind() == "secondary_constructor" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn unqualified_import(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    if visit.node.kind() != "import_header" {
        return Ok(());
    }
    let Some(target) = named_children(visit.node).find(|node| node.kind() == "identifier") else {
        return Ok(());
    };
    if node_text(context, target).contains('.') {
        return Ok(());
    }
    let alias = named_children(visit.node)
        .find(|node| node.kind() == "import_alias")
        .and_then(|alias| named_children(alias).next());
    super::bind_unknown(index, context, (visit, alias.or(Some(target))))
}

fn class_fence(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    input: (Visit<'_>, Option<&str>),
) -> Result<(), ExtractError> {
    let (visit, member) = input;
    let Some(class) = index
        .scopes
        .get(&visit.scope)
        .and_then(|scope| scope.nominal.as_ref())
    else {
        return Ok(());
    };
    record_non_method(
        context,
        &mut index.non_methods,
        (Some(class), member, false),
    )
}

fn singleton_factory(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
    visit: Visit<'_>,
) -> Result<(), ExtractError> {
    let own_class = index
        .scopes
        .get(&visit.scope)
        .and_then(|scope| scope.parent)
        .and_then(|parent| index.scopes.get(&parent))
        .is_some_and(|scope| scope.kind == ScopeKind::Class && scope.nominal.is_some());
    let own_self = visit
        .node
        .child_by_field_name("object")
        .is_some_and(|object| node_text(context, object) == "self");
    if own_class && own_self {
        return Ok(());
    }
    unknown_factory(index, context)
}

fn inside_singleton(
    context: &mut ExtractionContext<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let mut parent = node.parent();
    for _ in 0..=super::super::super::MAX_AST_DEPTH {
        context.ensure_active()?;
        let Some(node) = parent else {
            return Ok(false);
        };
        if node.kind() == "singleton_class" {
            return Ok(true);
        }
        if node.kind() == "class" {
            return Ok(false);
        }
        parent = node.parent();
    }
    Err(ExtractError::InvalidSpan)
}

fn unknown_factory(
    index: &mut SyntaxIndex<'_>,
    context: &mut ExtractionContext<'_, '_>,
) -> Result<(), ExtractError> {
    record_non_method(context, &mut index.non_methods, (None, Some("new"), true))
}
