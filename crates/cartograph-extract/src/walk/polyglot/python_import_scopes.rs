//! Conservative file-wide evidence for Python import fallback.
//! One syntax pass marks imported names with other bindings or attribute writes.
//! Reference marking uses indexed names and never descends from the tree root.

use std::{collections::HashMap, mem::size_of};

use tree_sitter::Node;

use crate::{
    ExtractError, PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX,
    walk::{ExtractionBuilder, ExtractionContext, MAX_BOUNDED_AST_VISITS},
};

const MAP_ENTRY_ALLOWANCE: u64 = 128;

#[derive(Clone, Copy)]
struct Visit<'tree> {
    node: Node<'tree>,
    depth: usize,
    target: bool,
    module_child: bool,
}

#[derive(Default)]
struct ImportNames {
    blocked: HashMap<String, bool>,
    block_all: bool,
    visits: usize,
}

pub(in crate::walk) fn fence_import_uses(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if builder.context.snapshot.language() != cartograph_domain::SourceLanguage::Python
        || builder.facts.import_bindings.is_empty()
    {
        return Ok(());
    }
    let mut names = ImportNames::default();
    for binding in &builder.facts.import_bindings {
        builder.context.ensure_active()?;
        names.insert(&mut builder.context, &binding.local_name)?;
    }
    names.scan(&mut builder.context, root)?;
    for reference in &mut builder.facts.references {
        names.fence_reference(&mut builder.context, reference)?;
    }
    Ok(())
}

impl ImportNames {
    fn insert(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        name: &str,
    ) -> Result<(), ExtractError> {
        if self.blocked.contains_key(name) {
            return Ok(());
        }
        context
            .budget
            .reserve_working_bytes(
                MAP_ENTRY_ALLOWANCE.saturating_add(
                    u64::try_from(size_of::<(String, bool)>())
                        .map_err(|_| ExtractError::OutputLimit)?,
                ),
            )?;
        self.blocked
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.blocked.insert(context.copy_text(name)?, false);
        Ok(())
    }

    fn scan(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        root: Node<'_>,
    ) -> Result<(), ExtractError> {
        let mut pending = Vec::new();
        push_node(
            context,
            &mut pending,
            Visit {
                node: root,
                depth: 0,
                target: false,
                module_child: false,
            },
        )?;
        while let Some(visit) = pending.pop() {
            context.ensure_active()?;
            self.visits = self.visits.saturating_add(1);
            if self.visits > MAX_BOUNDED_AST_VISITS || visit.depth > crate::MAXIMUM_AST_DEPTH {
                return Err(ExtractError::NestingLimit);
            }
            self.block_all |= matches!(visit.node.kind(), "wildcard_import" | "exec_statement");
            if visit.target && visit.node.kind() == "identifier" {
                let name = context
                    .source()
                    .get(visit.node.byte_range())
                    .unwrap_or_default();
                if let Some(blocked) = self.blocked.get_mut(name) {
                    *blocked = true;
                }
            }
            push_children(context, &mut pending, visit)?;
        }
        Ok(())
    }

    fn fence_reference(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        reference: &mut crate::ExtractedReference,
    ) -> Result<(), ExtractError> {
        context.ensure_active()?;
        let name = reference.name.split('.').next().unwrap_or_default();
        let blocked = self
            .blocked
            .get(name)
            .is_some_and(|blocked| *blocked || self.block_all);
        if reference.kind == cartograph_domain::ReferenceKind::Imports || !blocked {
            return Ok(());
        }
        let lookup = reference
            .resolution_name
            .as_deref()
            .unwrap_or(&reference.name);
        let resolution = format!("{PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX}{lookup}");
        context.budget.reserve_additional_string(&resolution)?;
        reference.resolution_name = Some(resolution);
        Ok(())
    }
}

fn push_children<'tree>(
    context: &mut ExtractionContext<'_, '_>,
    pending: &mut Vec<Visit<'tree>>,
    parent: Visit<'tree>,
) -> Result<(), ExtractError> {
    let mut cursor = parent.node.walk();
    if !cursor.goto_first_child() {
        return Ok(());
    }
    let mut first_named = true;
    loop {
        context.ensure_active()?;
        let node = cursor.node();
        if node.is_named() {
            let target = if parent.target {
                pattern_target(parent.node.kind(), (cursor.field_name(), first_named))
            } else {
                syntax_target(parent, cursor.field_name())
            };
            push_node(
                context,
                pending,
                Visit {
                    node,
                    depth: parent.depth.saturating_add(1),
                    target,
                    module_child: parent.node.kind() == "module",
                },
            )?;
            first_named &= node.is_extra();
        }
        if !cursor.goto_next_sibling() {
            return Ok(());
        }
    }
}

fn syntax_target(parent: Visit<'_>, field: Option<&str>) -> bool {
    match parent.node.kind() {
        "assignment"
        | "augmented_assignment"
        | "for_statement"
        | "for_in_clause"
        | "type_alias_statement" => field == Some("left"),
        "named_expression" => field == Some("name"),
        "function_definition" | "class_definition" => {
            matches!(field, Some("name" | "parameters" | "type_parameters"))
        }
        "lambda" => field == Some("parameters"),
        "as_pattern" | "except_clause" => field == Some("alias"),
        "global_statement" | "nonlocal_statement" | "delete_statement" | "case_pattern" => true,
        "import_statement" | "import_from_statement" => {
            // File-wide facts cannot prove uses of conditional or nested imports.
            field == Some("name") && !parent.module_child
        }
        _ => false,
    }
}

fn pattern_target(kind: &str, child: (Option<&str>, bool)) -> bool {
    let (field, first_named) = child;
    match kind {
        "attribute" => field == Some("object"),
        "subscript" => field == Some("value"),
        "call" => field == Some("function"),
        "typed_parameter" => field.is_none(),
        "default_parameter" | "typed_default_parameter" => field == Some("name"),
        "aliased_import" => field == Some("alias"),
        "dotted_name" => first_named,
        _ => true,
    }
}

fn push_node<'tree>(
    context: &mut ExtractionContext<'_, '_>,
    pending: &mut Vec<Visit<'tree>>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    context.budget.reserve_working_bytes(
        u64::try_from(size_of::<Visit<'_>>())
            .map_err(|_| ExtractError::OutputLimit)?
            .saturating_mul(2),
    )?;
    pending
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    pending.push(visit);
    Ok(())
}
