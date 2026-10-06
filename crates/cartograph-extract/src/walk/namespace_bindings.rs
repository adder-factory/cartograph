//! A write or shadow withdraws a Go namespace proof.
//! This deliberately fences the whole file, rather than inferring control flow.
use super::{
    ExtractionBuilder, MAX_AST_DEPTH, MAX_BOUNDED_AST_VISITS,
    script_support::LOAD_BINDING_LOCAL_NAME,
};
use crate::ExtractError;
use cartograph_domain::SourceLanguage;
use std::{collections::HashMap, mem::size_of};
use tree_sitter::Node;

pub(super) fn fence(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != SourceLanguage::Go {
        return Ok(());
    }
    let mut writes = HashMap::<&str, usize>::new();
    for binding in &builder.facts.import_bindings {
        builder.context.ensure_active()?;
        if binding.local_name == LOAD_BINDING_LOCAL_NAME {
            continue;
        }
        insert_name(
            &mut writes,
            &binding.local_name,
            &mut builder.context.budget,
        )?;
    }
    observe_writes(
        builder.context.source(),
        (root, &mut writes),
        &mut builder.context,
    )?;
    builder.context.budget.reserve_fact(
        u64::try_from(builder.facts.import_bindings.len())
            .map_err(|_| ExtractError::OutputLimit)?,
        [],
    )?;
    let mut bad_aliases = Vec::new();
    bad_aliases
        .try_reserve_exact(builder.facts.import_bindings.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    for binding in &builder.facts.import_bindings {
        builder.context.ensure_active()?;
        bad_aliases.push(
            writes
                .get(binding.local_name.as_str())
                .is_some_and(|count| *count != 0),
        );
    }
    drop(writes);
    for (binding, fenced) in builder.facts.import_bindings.iter_mut().zip(bad_aliases) {
        builder.context.ensure_active()?;
        if fenced {
            builder
                .context
                .budget
                .reserve_fact(0, [LOAD_BINDING_LOCAL_NAME])?;
            LOAD_BINDING_LOCAL_NAME.clone_into(&mut binding.local_name);
        }
    }
    Ok(())
}

fn insert_name<'a>(
    map: &mut HashMap<&'a str, usize>,
    name: &'a str,
    budget: &mut crate::budget::ExtractionBudget,
) -> Result<(), ExtractError> {
    if map.contains_key(name) {
        return Ok(());
    }
    budget.reserve_fact(
        u64::try_from(64 + size_of::<(&str, usize)>()).map_err(|_| ExtractError::OutputLimit)?,
        [],
    )?;
    map.try_reserve(1).map_err(|_| ExtractError::OutputLimit)?;
    map.insert(name, 0);
    Ok(())
}

fn observe_writes(
    source: &str,
    query: (Node<'_>, &mut HashMap<&str, usize>),
    context: &mut super::ExtractionContext<'_, '_>,
) -> Result<(), ExtractError> {
    let (root, writes) = query;
    let mut cursor = root.walk();
    let mut visited = 0_usize;
    let mut depth = 0_usize;
    loop {
        context.ensure_active()?;
        visited = visited.checked_add(1).ok_or(ExtractError::OutputLimit)?;
        if visited > MAX_BOUNDED_AST_VISITS || depth > MAX_AST_DEPTH {
            return Err(ExtractError::NestingLimit);
        }
        let node = cursor.node();
        if let Some(node) = written_name(node)
            && let Some(count) = source
                .get(node.byte_range())
                .and_then(|name| writes.get_mut(name))
        {
            *count = count.checked_add(1).ok_or(ExtractError::OutputLimit)?;
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
            depth = depth.saturating_sub(1);
        }
    }
}

fn written_name(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() != "identifier" {
        return None;
    }
    go_binding(node.parent()?).then_some(node)
}

fn go_binding(parent: Node<'_>) -> bool {
    if matches!(
        parent.kind(),
        "parameter_declaration"
            | "variadic_parameter_declaration"
            | "type_parameter_declaration"
            | "var_spec"
            | "const_spec"
    ) {
        return true;
    }
    if parent.kind() != "expression_list" {
        return false;
    }
    parent.parent().is_some_and(|owner| {
        matches!(
            owner.kind(),
            "short_var_declaration" | "range_clause" | "assignment_statement"
        ) && owner.child_by_field_name("left") == Some(parent)
    })
}
