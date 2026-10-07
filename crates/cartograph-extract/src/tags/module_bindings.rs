//! Retain explicit Elixir module qualifiers and bounded alias bindings that the
//! tag query's terminal-name captures cannot represent by themselves.
use tree_sitter::{Node, TreeCursor};

use super::{
    ExtractionBudget, MAX_TAG_AST_DEPTH, MAXIMUM_TAG_AST_NODES, MINIMUM_TAG_AST_NODES,
    TAG_AST_NODES_PER_SOURCE_BYTE, TagExtractionInput, span_for,
};
use crate::{
    ExtractError, ExtractedImportBinding, ImportBindingKind, budget::import_binding_budget_bytes,
};
use cartograph_domain::{SourceLanguage, SourcePosition, SourceSpan};
use std::{collections::HashSet, mem::size_of};

const MAXIMUM_NAME_BYTES: usize = 1_024;

pub(super) fn callee_binding(
    node: Node<'_>,
    source: &str,
    language: SourceLanguage,
) -> Result<Option<ExtractedImportBinding>, ExtractError> {
    let expected = match language {
        SourceLanguage::Elixir => "dot",
        _ => return Ok(None),
    };
    let Some(parent) = node.parent().filter(|parent| parent.kind() == expected) else {
        return Ok(None);
    };
    let value = source.get(parent.byte_range()).unwrap_or_default();
    let Some((module, member)) = value.rsplit_once('.') else {
        return Ok(None);
    };
    if !qualified_identifier(value) {
        return Ok(None);
    }
    Ok(Some(ExtractedImportBinding {
        kind: ImportBindingKind::Named,
        module_specifier: module.to_owned(),
        imported_name: member.to_owned(),
        local_name: member.to_owned(),
        span: span_for(node)?,
    }))
}

pub(super) fn push_binding(
    entry: (&mut Vec<ExtractedImportBinding>, ExtractedImportBinding),
    budget: &mut ExtractionBudget,
) -> Result<(), ExtractError> {
    let (bindings, binding) = entry;
    budget.reserve_fact(
        import_binding_budget_bytes(&binding),
        [
            binding.module_specifier.as_str(),
            binding.local_name.as_str(),
            binding.imported_name.as_str(),
        ],
    )?;
    bindings
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    bindings.push(binding);
    Ok(())
}

pub(super) fn public_members(
    input: (TagExtractionInput<'_, '_>, &[crate::ExtractedSymbol]),
    context: (&mut ExtractionBudget, &mut dyn FnMut() -> bool),
) -> Result<Vec<ExtractedImportBinding>, ExtractError> {
    let (input, symbols) = input;
    let (budget, cancelled) = context;
    let mut bindings = Vec::new();
    if input.snapshot.language() != SourceLanguage::Elixir {
        return Ok(bindings);
    }
    for symbol in symbols {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if !matches!(
            symbol.kind,
            cartograph_domain::SymbolKind::Function | cartograph_domain::SymbolKind::Method
        ) || symbol.qualified_name.len() > MAXIMUM_NAME_BYTES
        {
            continue;
        }
        let start =
            usize::try_from(symbol.span.start_byte()).map_err(|_| ExtractError::InvalidSpan)?;
        let end = usize::try_from(symbol.span.end_byte()).map_err(|_| ExtractError::InvalidSpan)?;
        let public = input
            .snapshot
            .source()
            .get(start..end)
            .and_then(|text| text.split_whitespace().next())
            .is_some_and(|keyword| {
                matches!(keyword, "def" | "defmacro" | "defdelegate" | "defguard")
            });
        if !public {
            continue;
        }
        push_binding(
            (
                &mut bindings,
                ExtractedImportBinding {
                    kind: ImportBindingKind::Namespace,
                    module_specifier: "<elixir-public-function>".to_owned(),
                    imported_name: "*".to_owned(),
                    local_name: symbol.qualified_name.clone(),
                    span: symbol.span,
                },
            ),
            budget,
        )?;
    }
    Ok(bindings)
}

pub(super) fn extract(
    input: TagExtractionInput<'_, '_>,
    budget: &mut ExtractionBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<ExtractedImportBinding>, ExtractError> {
    if !matches!(input.snapshot.language(), SourceLanguage::Elixir) {
        return Ok(Vec::new());
    }
    let mut cursor = input.root.walk();
    let mut bindings = Vec::new();
    let mut scopes = HashSet::<String>::new();
    let mut visited = 0_usize;
    let mut depth = 0_usize;
    let node_limit = input
        .snapshot
        .source()
        .len()
        .saturating_mul(TAG_AST_NODES_PER_SOURCE_BYTE)
        .saturating_add(MINIMUM_TAG_AST_NODES)
        .min(MAXIMUM_TAG_AST_NODES);
    loop {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        visited = visited.checked_add(1).ok_or(ExtractError::OutputLimit)?;
        if visited > node_limit || depth > MAX_TAG_AST_DEPTH {
            return Err(ExtractError::NestingLimit);
        }
        if let Some(binding) = binding((input, cursor.node()), &mut scopes, budget)? {
            push_binding((&mut bindings, binding), budget)?;
        }
        if !advance_cursor(&mut cursor, &mut depth) {
            return Ok(bindings);
        }
    }
}

/// Advance the depth-first walk, retaining depth while climbing to a sibling.
pub(super) fn advance_cursor(cursor: &mut TreeCursor<'_>, depth: &mut usize) -> bool {
    if cursor.goto_first_child() {
        *depth += 1;
        return true;
    }
    while !cursor.goto_next_sibling() {
        if !cursor.goto_parent() {
            return false;
        }
        *depth = depth.saturating_sub(1);
    }
    true
}

fn binding(
    (input, node): (TagExtractionInput<'_, '_>, Node<'_>),
    scopes: &mut HashSet<String>,
    budget: &mut ExtractionBudget,
) -> Result<Option<ExtractedImportBinding>, ExtractError> {
    let source = input.snapshot.source();
    if node.kind() != "call" {
        return Ok(None);
    }
    let target = node
        .child_by_field_name("target")
        .and_then(|target| source.get(target.byte_range()));
    if !matches!(target, Some("alias" | "require")) {
        return Ok(None);
    }
    let scope = span_for(enclosing_scope(node, source))?;
    if target == Some("require") {
        return Ok(source
            .get(node.byte_range())
            .unwrap_or_default()
            .contains("as:")
            .then(|| alias_fence(scope)));
    }
    let Some(binding) = elixir_alias(node, source)? else {
        return Ok(Some(alias_fence(scope)));
    };
    let root = binding
        .module_specifier
        .split('.')
        .next()
        .unwrap_or_default();
    if scopes.contains(root) {
        return Ok(Some(alias_fence(scope)));
    }
    reserve_alias(scopes, &binding.local_name, budget)?;
    Ok(Some(binding))
}

fn alias_fence(span: SourceSpan) -> ExtractedImportBinding {
    ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<elixir-alias-fence>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span,
    }
}

fn reserve_alias(
    names: &mut HashSet<String>,
    name: &str,
    budget: &mut ExtractionBudget,
) -> Result<(), ExtractError> {
    budget.reserve_fact(
        u64::try_from(64 + size_of::<String>() + name.len())
            .map_err(|_| ExtractError::OutputLimit)?,
        [name],
    )?;
    names
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    names.insert(name.to_owned());
    Ok(())
}

fn elixir_alias(
    node: Node<'_>,
    source: &str,
) -> Result<Option<ExtractedImportBinding>, ExtractError> {
    let Some(target) = node.child_by_field_name("target") else {
        return Ok(None);
    };
    if source.get(target.byte_range()) != Some("alias") {
        return Ok(None);
    }
    let Some(arguments) = node.named_child(1) else {
        return Ok(None);
    };
    let Some(module) = arguments
        .named_child(0)
        .filter(|node| node.kind() == "alias")
    else {
        return Ok(None);
    };
    let value = source.get(module.byte_range()).unwrap_or_default();
    if !qualified_identifier(value) {
        return Ok(None);
    }
    let Some(local) = alias_local_name(arguments, source, value) else {
        return Ok(None);
    };
    Ok(Some(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: value.to_owned(),
        imported_name: "*".to_owned(),
        local_name: local.to_owned(),
        span: binding_scope(node, source)?,
    }))
}

fn alias_local_name<'source>(
    arguments: Node<'_>,
    source: &'source str,
    module: &'source str,
) -> Option<&'source str> {
    match arguments.named_child_count() {
        1 => module.rsplit('.').next(),
        2 => {
            let options = arguments.named_child(1)?;
            let alias = source
                .get(options.byte_range())?
                .strip_prefix("as:")?
                .trim();
            (qualified_identifier(alias) && !alias.contains('.')).then_some(alias)
        }
        _ => None,
    }
}

fn binding_scope(node: Node<'_>, source: &str) -> Result<SourceSpan, ExtractError> {
    let start = span_for(node)?;
    let end = span_for(enclosing_scope(node, source))?;
    let first = SourcePosition::new(start.start_byte(), start.start_line(), start.start_column())
        .map_err(|_| ExtractError::InvalidSpan)?;
    let last = SourcePosition::new(end.end_byte(), end.end_line(), end.end_column())
        .map_err(|_| ExtractError::InvalidSpan)?;
    SourceSpan::new(first, last).map_err(|_| ExtractError::InvalidSpan)
}

fn enclosing_scope<'tree>(mut node: Node<'tree>, source: &str) -> Node<'tree> {
    let original = node;
    for _ in 0..MAX_TAG_AST_DEPTH {
        let Some(parent) = node.parent() else {
            return node;
        };
        node = parent;
        let target = node
            .child_by_field_name("target")
            .and_then(|target| source.get(target.byte_range()));
        if matches!(
            node.kind(),
            "do_block" | "anonymous_function" | "stab_clause"
        ) || target.is_some_and(|target| {
            matches!(
                target,
                "defmodule" | "def" | "defp" | "defmacro" | "defmacrop"
            )
        }) {
            return node;
        }
    }
    original
}

fn qualified_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_NAME_BYTES
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.chars().all(|character| {
                    character.is_alphanumeric() || matches!(character, '_' | '?' | '!')
                })
        })
}
