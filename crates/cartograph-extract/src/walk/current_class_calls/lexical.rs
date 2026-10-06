//! One conservative, callee-specific check over the complete enclosing callable.

use std::collections::{HashMap, HashSet};

use cartograph_domain::SourceLanguage;
use tree_sitter::Node;

use super::super::ExtractionBuilder;
use crate::ExtractError;

#[derive(Default)]
pub(in super::super) struct MethodProofIndex {
    methods: HashMap<usize, MethodEvidence>,
}

struct MethodEvidence {
    non_calls: HashSet<String>,
    this_parameter: bool,
}

fn folded(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::VbNet | SourceLanguage::Vb6 | SourceLanguage::Pascal | SourceLanguage::Sql
    )
}

fn identifier(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$')
}

fn token_name(text: &str, fold: bool) -> Result<String, ExtractError> {
    let mut name = String::new();
    name.try_reserve(text.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    name.push_str(text);
    if fold {
        name.make_ascii_lowercase();
    }
    Ok(name)
}

fn scan(
    builder: &mut ExtractionBuilder<'_, '_>,
    method: Node<'_>,
) -> Result<HashSet<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let text = source.get(method.byte_range()).unwrap_or_default();
    let fold = folded(builder.context.snapshot.language());
    let mut tokens = text.char_indices().peekable();
    let mut non_calls = HashSet::new();
    while let Some((start, character)) = tokens.next() {
        builder.context.ensure_active()?;
        if !identifier(character) {
            continue;
        }
        while tokens
            .peek()
            .is_some_and(|(_, character)| identifier(*character))
        {
            builder.context.ensure_active()?;
            tokens.next();
        }
        let end = tokens.peek().map_or(text.len(), |(offset, _)| *offset);
        while tokens
            .peek()
            .is_some_and(|(_, character)| character.is_whitespace())
        {
            builder.context.ensure_active()?;
            tokens.next();
        }
        if tokens
            .peek()
            .is_some_and(|(_, character)| *character == '(')
        {
            continue;
        }
        builder.context.budget.reserve_working_bytes(
            128_u64.saturating_add(u64::try_from(end - start).unwrap_or(u64::MAX)),
        )?;
        let name = token_name(&text[start..end], fold)?;
        non_calls
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        non_calls.insert(name);
    }
    Ok(non_calls)
}

fn cache_method(
    builder: &mut ExtractionBuilder<'_, '_>,
    method: Node<'_>,
) -> Result<(), ExtractError> {
    if builder
        .current_class_calls
        .methods
        .contains_key(&method.id())
    {
        return Ok(());
    }
    let this_parameter = has_this_parameter(builder, method)?;
    let non_calls = scan(builder, method)?;
    builder.context.budget.reserve_working_bytes(128)?;
    builder
        .current_class_calls
        .methods
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    builder.current_class_calls.methods.insert(
        method.id(),
        MethodEvidence {
            non_calls,
            this_parameter,
        },
    );
    Ok(())
}

pub(super) fn clean_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    method: Node<'_>,
    name: &str,
) -> Result<bool, ExtractError> {
    // Non-ASCII spellings need a language's full identifier rules; abstain.
    if !name.is_ascii() {
        return Ok(false);
    }
    cache_method(builder, method)?;
    let name = token_name(name, folded(builder.context.snapshot.language()))?;
    Ok(builder
        .current_class_calls
        .methods
        .get(&method.id())
        .is_some_and(|evidence| !evidence.non_calls.contains(&name)))
}

fn has_this_parameter(
    builder: &mut ExtractionBuilder<'_, '_>,
    method: Node<'_>,
) -> Result<bool, ExtractError> {
    let Some(parameters) = method.child_by_field_name("parameters") else {
        return Ok(false);
    };
    let source = builder.context.snapshot.source();
    let text = source.get(parameters.byte_range()).unwrap_or_default();
    for token in text.split(|character: char| !identifier(character)) {
        builder.context.ensure_active()?;
        if token == "this" {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn explicit_this_parameter(
    builder: &mut ExtractionBuilder<'_, '_>,
    method: Node<'_>,
) -> Result<bool, ExtractError> {
    cache_method(builder, method)?;
    Ok(builder
        .current_class_calls
        .methods
        .get(&method.id())
        .is_none_or(|evidence| evidence.this_parameter))
}
