//! A source command needs script execution scope and a stable working directory.
use super::{ExtractionBuilder, MAX_AST_DEPTH, MAX_BOUNDED_AST_VISITS};
use crate::{ExtractError, ImportBindingKind};
use cartograph_domain::SourceLanguage;
use std::mem::size_of;
use tree_sitter::Node;

pub(super) fn top_level(mut node: Node<'_>) -> bool {
    for _ in 0..MAX_AST_DEPTH {
        let Some(parent) = node.parent() else {
            return true;
        };
        if parent.parent().is_none() {
            return true;
        }
        if !matches!(
            parent.kind(),
            "statement_list" | "pipeline" | "pipeline_chain" | "job" | "list"
        ) {
            return false;
        }
        if parent.kind() != "statement_list" && parent.named_child_count() != 1 {
            return false;
        }
        node = parent;
    }
    false
}

pub(super) fn unknown_source(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    builder.emit_import_binding(crate::ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<shell-unknown-source>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: super::syntax::span_for(node)?,
    })
}

pub(super) fn fence(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if !matches!(
        builder.context.snapshot.language(),
        SourceLanguage::Bash
            | SourceLanguage::Zsh
            | SourceLanguage::Fish
            | SourceLanguage::PowerShell
    ) {
        return Ok(());
    }
    let Some(first) = first_directory_change(builder, root)? else {
        return Ok(());
    };
    builder.context.budget.reserve_fact(
        u64::try_from(
            builder
                .facts
                .import_bindings
                .len()
                .saturating_mul(size_of::<crate::ExtractedImportBinding>()),
        )
        .map_err(|_| ExtractError::OutputLimit)?,
        [],
    )?;
    let mut retained = Vec::new();
    retained
        .try_reserve_exact(builder.facts.import_bindings.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    for binding in std::mem::take(&mut builder.facts.import_bindings) {
        builder.context.ensure_active()?;
        if binding.kind != ImportBindingKind::Namespace
            || binding.local_name != "*"
            || binding.span.start_byte() < first
        {
            retained.push(binding);
        }
    }
    builder.facts.import_bindings = retained;
    builder.emit_import_binding(crate::ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<shell-cwd-change>".to_owned(),
        imported_name: "*".to_owned(),
        local_name: "*".to_owned(),
        span: super::syntax::span_for(root)?,
    })
}

fn first_directory_change(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<Option<u64>, ExtractError> {
    let mut cursor = root.walk();
    let mut visited = 0_usize;
    let mut depth = 0_usize;
    let mut first = None;
    loop {
        builder.context.ensure_active()?;
        visited = visited.checked_add(1).ok_or(ExtractError::OutputLimit)?;
        if visited > MAX_BOUNDED_AST_VISITS || depth > MAX_AST_DEPTH {
            return Err(ExtractError::NestingLimit);
        }
        let node = cursor.node();
        let text = builder.context.text(node);
        if text.len() <= 32
            && [
                "cd",
                "pushd",
                "popd",
                "Set-Location",
                "Push-Location",
                "Pop-Location",
                "sl",
                "chdir",
            ]
            .iter()
            .any(|name| text.eq_ignore_ascii_case(name))
        {
            let position =
                u64::try_from(node.start_byte()).map_err(|_| ExtractError::OutputLimit)?;
            first = Some(first.map_or(position, |current: u64| current.min(position)));
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(first);
            }
            depth = depth.saturating_sub(1);
        }
    }
}
