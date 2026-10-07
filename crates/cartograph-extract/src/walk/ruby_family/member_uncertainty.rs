//! Unmodelled Ruby method changes fence the existing receiver ancestry owner.

use super::super::{
    AstVisitBudget, ExtractionBuilder, MAX_AST_DEPTH, explicit_receivers, syntax::named_children,
};
use crate::ExtractError;
use cartograph_domain::SymbolId;
use tree_sitter::Node;

const MAX_METHOD_NAME_BYTES: usize = 512;
const INCLUDE_METHODS: [&str; 2] = ["include", "extend"];
const MISSING_METHODS: [&str; 2] = ["method_missing", "respond_to_missing?"];
const SINGLE_NAME_MUTATORS: [&str; 2] = ["alias_method", "define_method"];
const MULTI_NAME_MUTATORS: [&str; 2] = ["undef_method", "remove_method"];

pub(super) fn capture(
    builder: &mut ExtractionBuilder<'_, '_>,
    (root, owner): (Node<'_>, &SymbolId),
) -> Result<bool, ExtractError> {
    let mut cursor = root.walk();
    let mut budget = AstVisitBudget::<MAX_AST_DEPTH>::default();
    let mut depth = 0_usize;
    let mut include_blocked = false;
    loop {
        builder.context.ensure_active()?;
        budget.observe(builder, depth)?;
        let node = cursor.node();
        let nested = node != root && matches!(node.kind(), "class" | "module");
        if !nested {
            include_blocked |= observe(builder, (node, owner))?;
        }
        if !nested && cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(include_blocked);
            }
            depth = depth.saturating_sub(1);
        }
    }
}

fn observe(
    builder: &mut ExtractionBuilder<'_, '_>,
    (node, owner): (Node<'_>, &SymbolId),
) -> Result<bool, ExtractError> {
    if !node.is_named() {
        return Ok(false);
    }
    match node.kind() {
        "method" | "singleton_method" => definition(builder, (node, owner)),
        "alias" => affected(builder, (node.child_by_field_name("name"), owner)),
        "undef" => affected_names(builder, (node, owner)),
        "call" => mutation(builder, (node, owner)),
        "ERROR" => {
            fence(builder, (owner, None))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    (node, owner): (Node<'_>, &SymbolId),
) -> Result<bool, ExtractError> {
    let name = node
        .child_by_field_name("name")
        .map(|name| builder.context.text(name));
    let missing = name.is_some_and(|name| MISSING_METHODS.contains(&name));
    let include_blocked = name.is_some_and(|name| INCLUDE_METHODS.contains(&name));
    if missing {
        fence(builder, (owner, None))?;
    }
    Ok(include_blocked)
}

fn mutation(
    builder: &mut ExtractionBuilder<'_, '_>,
    (node, owner): (Node<'_>, &SymbolId),
) -> Result<bool, ExtractError> {
    let Some(method) = node.child_by_field_name("method") else {
        return Ok(false);
    };
    let name = builder.context.text(method);
    if MISSING_METHODS.contains(&name) {
        fence(builder, (owner, None))?;
        return Ok(false);
    }
    let single = SINGLE_NAME_MUTATORS.contains(&name);
    if !single && !MULTI_NAME_MUTATORS.contains(&name) {
        return Ok(false);
    }
    let arguments = node.child_by_field_name("arguments");
    if single {
        return affected(
            builder,
            (
                arguments.and_then(|args| named_children(args).next()),
                owner,
            ),
        );
    }
    match arguments {
        Some(arguments) => affected_names(builder, (arguments, owner)),
        None => affected(builder, (None, owner)),
    }
}

fn affected_names(
    builder: &mut ExtractionBuilder<'_, '_>,
    (node, owner): (Node<'_>, &SymbolId),
) -> Result<bool, ExtractError> {
    let mut blocked = false;
    for name in named_children(node) {
        builder.context.ensure_active()?;
        blocked |= affected(builder, (Some(name), owner))?;
    }
    Ok(blocked)
}

fn affected(
    builder: &mut ExtractionBuilder<'_, '_>,
    (node, owner): (Option<Node<'_>>, &SymbolId),
) -> Result<bool, ExtractError> {
    let name = node.and_then(|node| literal_name(builder, node));
    let all = name.is_none_or(|name| MISSING_METHODS.contains(&name));
    fence(builder, (owner, if all { None } else { name }))?;
    Ok(name.is_none_or(|name| INCLUDE_METHODS.contains(&name)))
}

fn literal_name<'source>(
    builder: &ExtractionBuilder<'source, '_>,
    node: Node<'_>,
) -> Option<&'source str> {
    let text = builder.context.snapshot.source().get(node.byte_range())?;
    let name = match node.kind() {
        "simple_symbol" => text.strip_prefix(':')?,
        "string" => text
            .strip_prefix('"')
            .and_then(|name| name.strip_suffix('"'))
            .or_else(|| {
                text.strip_prefix('\'')
                    .and_then(|name| name.strip_suffix('\''))
            })?,
        "identifier"
            if node
                .parent()
                .is_some_and(|parent| matches!(parent.kind(), "alias" | "undef")) =>
        {
            text
        }
        _ => return None,
    };
    (!name.is_empty()
        && name.len() <= MAX_METHOD_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_!?=".contains(&byte)))
    .then_some(name)
}

fn fence(
    builder: &mut ExtractionBuilder<'_, '_>,
    (owner, name): (&SymbolId, Option<&str>),
) -> Result<(), ExtractError> {
    explicit_receivers::record_non_method(
        &mut builder.context,
        &mut builder.facts.receiver_bindings,
        (Some(owner), name, false),
    )
}
